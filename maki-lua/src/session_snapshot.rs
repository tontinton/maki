//! The one shape `maki.session.read()` returns. The UI and the headless
//! drivers all serialize this struct, so a plugin sees the same keys wherever
//! it runs and a new field is one edit instead of three `json!` blobs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use maki_agent::agent::live_header;
use maki_agent::session::{SessionHeader, StoredSession};
use maki_agent::{AgentEvent, Envelope};
use maki_providers::{TokenUsage, add_cost};
use maki_storage::id::{MakiId, SessionRef};
use maki_storage::sessions::StoredMode;
use serde::Serialize;

use crate::EventHandle;
use crate::api::session::{SessionSnapshotFn, not_live};

pub const MODE_BUILD: &str = "build";
pub const MODE_PLAN: &str = "plan";
pub const STATUS_IDLE: &str = "idle";
pub const STATUS_WORKING: &str = "working";
pub const STATUS_NEEDS_INPUT: &str = "needs_input";
/// A session no runtime here has open, read from disk.
pub const STATUS_STORED: &str = "stored";
/// ACP runs many sessions and none is the default, so an id-less ask outside
/// a tool call has nothing to answer for.
pub const NO_CALLING_SESSION_ERR: &str =
    "no calling session: pass `session`, events carry it as `session_id`";

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
    /// Spelled like the thinking setting. Left out by hosts that do not
    /// track it (the headless drivers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast: Option<bool>,
    /// [`STATUS_IDLE`], [`STATUS_WORKING`], [`STATUS_NEEDS_INPUT`], or
    /// [`STATUS_STORED`].
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
    /// Left out for a stored session: the window belongs to a model that is
    /// only resolved when the session opens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// What the session bills, `None` on an unpriced model. No list price
    /// twin here, because `cost` is re-settled from stored usage when a
    /// session resumes and list price is not stored. Per turn list price
    /// lives on the `TurnEnd` autocmd instead.
    pub cost: Option<f64>,
}

impl SessionSnapshot {
    /// What the file says. Nothing runs it here, so no cost: that is settled
    /// against the model's rates when the session opens.
    pub fn stored(session: &StoredSession) -> Self {
        Self::unhosted(session.id, SessionHeader::of(session), STATUS_STORED)
    }

    /// A session a headless driver runs, answered from what it publishes
    /// rather than from a file it may not have written yet.
    pub fn live_headless(id: MakiId, header: SessionHeader) -> Self {
        let status = if header.working {
            STATUS_WORKING
        } else {
            STATUS_IDLE
        };
        Self::unhosted(id, header, status)
    }

    /// A session this host holds no runtime for: no window, cost or queue.
    fn unhosted(id: MakiId, header: SessionHeader, status: &'static str) -> Self {
        Self {
            id: id.to_string(),
            cwd: header.cwd,
            title: Some(header.title),
            model: header.model,
            mode: match header.mode {
                Some(StoredMode::Plan) => MODE_PLAN,
                _ => MODE_BUILD,
            },
            thinking: header.thinking.map(|thinking| thinking.to_string()),
            fast: Some(header.fast),
            status,
            focused: false,
            updated_at: header.updated_at,
            queue: None,
            usage: header.usage,
            context_size: header.context_size,
            context_window: None,
            cost: None,
        }
    }
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
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Backs `maki.session.read` for a session a headless driver runs (`maki -p`,
/// sdk mode, each ACP session). The driver writes while the Lua thread reads,
/// hence the lock, though the two rarely meet.
#[derive(Clone, Default)]
pub struct HeadlessSnapshot(Arc<Mutex<Totals>>);

impl HeadlessSnapshot {
    /// A session that has already spent and has a window before its first
    /// turn, like a resumed one.
    pub fn starting_at(cost: Option<f64>, context_window: u32) -> Self {
        Self(Arc::new(Mutex::new(Totals {
            cost,
            context_window,
            ..Totals::default()
        })))
    }

    /// Fold one envelope in. The drivers already loop over the stream, so
    /// they hand every envelope here and keep no bookkeeping of their own.
    pub fn observe(&self, envelope: &Envelope) {
        let event = &envelope.event;
        let mut totals = lock(&self.0);
        // Envelopes only flow while a run is alive, so anything that is not
        // an ending means the agent is busy. Sdk mode runs many turns and no
        // driver has to remember to clear a flag between them.
        totals.working = !matches!(event, AgentEvent::Done { .. } | AgentEvent::Error { .. });
        // Each run ending carries that run's whole bill, subagents and
        // compaction included, so summing them lands on the exact session
        // total. A failed run paid for its rounds too. Summing `TurnComplete`
        // would miss compaction.
        match event {
            AgentEvent::Done {
                usage,
                cost,
                context_size,
                context_window,
                ..
            } => {
                totals.usage += *usage;
                add_cost(&mut totals.cost, *cost);
                totals.context_size = *context_size;
                totals.context_window = *context_window;
            }
            AgentEvent::Error { totals: run, .. } => {
                totals.usage += run.usage;
                add_cost(&mut totals.cost, run.cost);
            }
            AgentEvent::TurnComplete(turn) if envelope.subagent.is_none() => {
                totals.context_size = turn.context_size.unwrap_or(totals.context_size);
                totals.context_window = turn.context_window;
            }
            _ => {}
        }
    }

    /// Install as the provider `maki.session.read` answers from, for the
    /// single session drivers.
    pub fn install(
        &self,
        handle: &EventHandle,
        meta: HeadlessMeta,
        mode: impl Fn() -> &'static str + Send + Sync + 'static,
    ) {
        handle.install_session_snapshot(Box::new(self.provider(meta, mode)));
    }

    /// `mode` is a callback because sdk mode and ACP can flip between build
    /// and plan mid-run.
    ///
    /// There is no stored session and no rename path under these drivers, so
    /// `title` stays nil rather than echoing the id back as a fake one.
    fn provider(
        &self,
        meta: HeadlessMeta,
        mode: impl Fn() -> &'static str + Send + Sync + 'static,
    ) -> impl Fn(Option<&str>) -> Result<serde_json::Value, String> + Send + Sync + 'static {
        let totals = Arc::clone(&self.0);
        // A legacy hex id and its base58 form name the same session.
        let own = meta.id.parse::<MakiId>().ok();
        move |id| {
            if let Some(want) = id
                && want != meta.id
                && (own.is_none() || want.parse::<MakiId>().ok() != own)
            {
                return Err(not_live(want));
            }
            let mode = mode();
            let totals = lock(&totals);
            serde_json::to_value(SessionSnapshot {
                id: meta.id.clone(),
                cwd: meta.cwd.clone(),
                title: None,
                model: meta.model.clone(),
                mode,
                thinking: None,
                fast: None,
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
                context_window: Some(totals.context_window),
                cost: totals.cost,
            })
            .map_err(|e| e.to_string())
        }
    }

    /// What the session publishes (title, thinking, fast, the model it runs,
    /// usage), with the cost and window only this tally has on top. `mode` is
    /// a callback because the published one lags until the next turn.
    fn hosted_provider(
        &self,
        session: SessionRef,
        mode: impl Fn() -> &'static str + Send + Sync + 'static,
    ) -> Provider {
        let totals = Arc::clone(&self.0);
        Arc::new(move || {
            let id = session.id();
            let header = live_header(id).ok_or_else(|| not_live(&session))?;
            let mut snapshot = SessionSnapshot::live_headless(id, Arc::unwrap_or_clone(header));
            let totals = lock(&totals);
            snapshot.id = session.to_string();
            snapshot.mode = mode();
            snapshot.context_window = Some(totals.context_window);
            snapshot.cost = totals.cost;
            serde_json::to_value(snapshot).map_err(|e| e.to_string())
        })
    }
}

type Provider = Arc<dyn Fn() -> Result<serde_json::Value, String> + Send + Sync>;
type Providers = Arc<Mutex<HashMap<MakiId, Provider>>>;

/// Backs `maki.session.read` for ACP, which runs many sessions on one plugin
/// host. Each answers only for its own id, so no session can stand in for
/// another, and a closed one leaves nothing behind.
#[derive(Clone, Default)]
pub struct HeadlessSessions(Providers);

impl HeadlessSessions {
    pub fn install(&self, handle: &EventHandle) {
        handle.install_session_snapshot(self.provider());
    }

    pub fn provider(&self) -> SessionSnapshotFn {
        let providers = Arc::clone(&self.0);
        Box::new(move |id| {
            let want = id.ok_or_else(|| NO_CALLING_SESSION_ERR.to_owned())?;
            let unknown = || not_live(want);
            let session = want.parse::<MakiId>().map_err(|_| unknown())?;
            let provider = lock(&providers)
                .get(&session)
                .cloned()
                .ok_or_else(unknown)?;
            provider()
        })
    }

    /// Answers for `session` until the returned guard drops.
    #[must_use = "dropping the guard unregisters the session at once"]
    pub fn register(
        &self,
        session: SessionRef,
        snapshot: &HeadlessSnapshot,
        mode: impl Fn() -> &'static str + Send + Sync + 'static,
    ) -> RegisteredSession {
        let id = session.id();
        let provider = snapshot.hosted_provider(session, mode);
        lock(&self.0).insert(id, Arc::clone(&provider));
        RegisteredSession {
            providers: Arc::clone(&self.0),
            id,
            provider,
        }
    }
}

/// Unregisters on drop, so every way a session ends drops it. A reload that
/// registered again first keeps its own entry.
pub struct RegisteredSession {
    providers: Providers,
    id: MakiId,
    provider: Provider,
}

impl Drop for RegisteredSession {
    fn drop(&mut self) {
        let mut providers = lock(&self.providers);
        if providers
            .get(&self.id)
            .is_some_and(|live| Arc::ptr_eq(live, &self.provider))
        {
            providers.remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use arc_swap::ArcSwap;
    use maki_agent::agent::{HistorySnapshot, SharedHeader, SharedMessages, publish_live_session};
    use maki_agent::{DoneReason, RunTotals, TurnCompleteEvent};
    use maki_providers::Message;

    use super::*;

    const FIRST_TURN_INPUT: u32 = 1_000;
    const SECOND_TURN_INPUT: u32 = 400;
    const TURN_COST: f64 = 0.25;
    const WINDOW: u32 = 200_000;
    const TITLE: &str = "fix the build";
    const MODEL: &str = "anthropic/claude-sonnet-4-5";
    const CWD: &str = "/work/repo";
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
            subsidised_list_cost: None,
            context_size: Some(context_size),
            context_window: WINDOW,
        })))
    }

    fn done(input: u32) -> Envelope {
        envelope(AgentEvent::Done {
            usage: TokenUsage {
                input: u64::from(input),
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
            u64::from(FIRST_TURN_INPUT + SECOND_TURN_INPUT),
            "{GROWS_MSG}"
        );
        assert_eq!(totals.cost, Some(TURN_COST * 2.0), "{GROWS_MSG}");
        assert_eq!(totals.context_size, SECOND_TURN_INPUT);
    }

    fn meta(id: String) -> HeadlessMeta {
        HeadlessMeta {
            id,
            cwd: String::new(),
            model: String::new(),
        }
    }

    /// Keep the returned pair alive for as long as the session should be.
    fn publish(id: MakiId) -> (SharedMessages, SharedHeader) {
        let header = SessionHeader::of(&StoredSession::new("", ""));
        let header: SharedHeader = Arc::new(ArcSwap::from_pointee(header));
        let mirror: SharedMessages = Arc::new(ArcSwap::from_pointee(HistorySnapshot::default()));
        publish_live_session(id, &mirror, &header);
        (mirror, header)
    }

    /// A reload registers the same id before the old run's guard drops, and
    /// that late drop must not unregister the new run.
    #[test]
    fn a_stale_guard_leaves_the_reloaded_session_registered() {
        let sessions = HeadlessSessions::default();
        let read = sessions.provider();
        let session = SessionRef::generate();
        let _live = publish(session.id());
        let register =
            || sessions.register(session.clone(), &HeadlessSnapshot::default(), || MODE_BUILD);
        let old = register();
        let new = register();

        drop(old);

        assert!(read(Some(session.as_str())).is_ok());
        drop(new);
        assert_eq!(
            read(Some(session.as_str())).unwrap_err(),
            not_live(&session)
        );
    }

    /// The window and cost are the pump's tally, the rest what the session
    /// publishes, so a read keeps every field it had before ACP had a tally.
    #[test]
    fn a_hosted_read_lays_the_tally_over_the_published_header() {
        let session = SessionRef::generate();
        let (_mirror, header) = publish(session.id());
        header.store(Arc::new(SessionHeader {
            title: TITLE.into(),
            fast: true,
            ..SessionHeader::of(&StoredSession::new(MODEL, CWD))
        }));
        let snapshot = HeadlessSnapshot::default();
        snapshot.observe(&done(FIRST_TURN_INPUT));
        let sessions = HeadlessSessions::default();
        let _registered = sessions.register(session.clone(), &snapshot, || MODE_PLAN);

        let read = sessions.provider()(Some(session.as_str())).unwrap();

        assert_eq!(read["title"], TITLE);
        assert_eq!(read["fast"], true);
        assert_eq!(read["model"], MODEL);
        assert_eq!(read["focused"], false);
        assert_eq!(read["mode"], MODE_PLAN);
        assert_eq!(read["cost"], TURN_COST);
        assert_eq!(read["context_window"], WINDOW);
    }

    /// A resumed session has spent and has a window before its first new turn,
    /// and what it spends next adds to that.
    #[test]
    fn a_resumed_session_reads_its_restored_bill_and_adds_to_it() {
        const RESTORED_COST: f64 = 3.0;
        let session = SessionRef::generate();
        let _live = publish(session.id());
        let snapshot = HeadlessSnapshot::starting_at(Some(RESTORED_COST), WINDOW);
        let sessions = HeadlessSessions::default();
        let _registered = sessions.register(session.clone(), &snapshot, || MODE_BUILD);
        let read = || sessions.provider()(Some(session.as_str())).unwrap();

        assert_eq!(read()["cost"], RESTORED_COST);
        assert_eq!(read()["context_window"], WINDOW);

        snapshot.observe(&done(FIRST_TURN_INPUT));

        assert_eq!(read()["cost"], RESTORED_COST + TURN_COST);
    }

    /// Lineage carries the base58 form, a resumed legacy session its hex one.
    #[test]
    fn a_legacy_hex_session_answers_to_its_base58_id() {
        let id = MakiId::generate();
        let hex: String = id.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex.parse::<MakiId>().unwrap(), id);
        let read = HeadlessSnapshot::default().provider(meta(hex), || MODE_BUILD);

        assert!(read(Some(&id.to_string())).is_ok());
        let stranger = MakiId::generate().to_string();
        assert_eq!(read(Some(&stranger)).unwrap_err(), not_live(&stranger));
    }

    #[test]
    fn failed_run_bill_joins_the_session_totals() {
        let snapshot = HeadlessSnapshot::default();
        snapshot.observe(&done(FIRST_TURN_INPUT));

        snapshot.observe(&envelope(AgentEvent::Error {
            message: String::new(),
            auth: false,
            totals: RunTotals {
                usage: TokenUsage {
                    input: u64::from(SECOND_TURN_INPUT),
                    ..Default::default()
                },
                cost: Some(TURN_COST),
                list_cost: None,
            },
        }));

        let totals = lock(&snapshot.0);
        assert!(!totals.working);
        assert_eq!(
            totals.usage.input,
            u64::from(FIRST_TURN_INPUT + SECOND_TURN_INPUT)
        );
        assert_eq!(totals.cost, Some(TURN_COST * 2.0));
        assert_eq!(totals.context_size, FIRST_TURN_INPUT);
    }
}
