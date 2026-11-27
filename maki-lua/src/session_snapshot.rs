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

use crate::api::session::{SessionSnapshotFn, not_live};
use crate::headless::SessionOpening;

pub const MODE_BUILD: &str = "build";
pub const MODE_PLAN: &str = "plan";
pub const STATUS_IDLE: &str = "idle";
pub const STATUS_WORKING: &str = "working";
pub const STATUS_NEEDS_INPUT: &str = "needs_input";
/// A session no runtime here has open, read from disk.
pub const STATUS_STORED: &str = "stored";
/// ACP runs many sessions and none is the default, so an id-less ask from
/// outside a session's tool calls and event handlers has nothing to answer for.
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
    /// Spelled like the thinking setting.
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

#[derive(Default)]
struct Totals {
    usage: TokenUsage,
    cost: Option<f64>,
    context_size: u32,
    context_window: u32,
    working: bool,
}

impl Totals {
    fn status(&self) -> &'static str {
        if self.working {
            STATUS_WORKING
        } else {
            STATUS_IDLE
        }
    }
}

/// A poisoned lock only means some other run panicked mid-update. The
/// numbers are still whole, so read them instead of losing the session's
/// whole accounting over it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The running tally of a session a headless driver runs (`maki -p`, sdk
/// mode, each ACP session). The driver writes while the Lua thread reads,
/// hence the lock, though the two rarely meet.
#[derive(Clone, Default)]
pub(crate) struct HeadlessSnapshot(Arc<Mutex<Totals>>);

/// Starts from what the session spent before this process opened it.
impl From<&SessionOpening> for HeadlessSnapshot {
    fn from(opening: &SessionOpening) -> Self {
        Self(Arc::new(Mutex::new(Totals {
            usage: opening.usage,
            cost: opening.cost,
            context_size: opening.context_size,
            context_window: opening.context_window,
            working: false,
        })))
    }
}

impl HeadlessSnapshot {
    /// Fold one envelope in. The drivers already loop over the stream, so
    /// they hand every envelope here and keep no bookkeeping of their own.
    pub(crate) fn observe(&self, envelope: &Envelope) {
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

    /// What the session publishes (title, thinking, fast, the model it runs),
    /// with the tally on top. The agent republishes the header only after the
    /// turn's `Done` is out, so a `TurnEnd` handler reading it would find the
    /// turn still working on the old usage. `mode` is a callback because the
    /// published one lags until the next turn. Until a header is published
    /// (a failed connect never does) `opening` answers, and `cwd` is always
    /// the driver's own, since a stored session keeps the one it started in.
    fn provider(
        &self,
        session: SessionRef,
        opening: SessionHeader,
        cwd: String,
        focused: bool,
        mode: impl Fn() -> &'static str + Send + Sync + 'static,
    ) -> Provider {
        let totals = Arc::clone(&self.0);
        Arc::new(move || {
            let id = session.id();
            let header = live_header(id).map_or_else(|| opening.clone(), Arc::unwrap_or_clone);
            let totals = lock(&totals);
            let mut snapshot = SessionSnapshot::unhosted(id, header, totals.status());
            snapshot.id = session.to_string();
            snapshot.cwd.clone_from(&cwd);
            if snapshot.model.is_empty() {
                snapshot.model.clone_from(&opening.model);
            }
            snapshot.mode = mode();
            snapshot.focused = focused;
            snapshot.usage = totals.usage;
            snapshot.context_size = totals.context_size;
            snapshot.context_window = Some(totals.context_window);
            snapshot.cost = totals.cost;
            serde_json::to_value(snapshot).map_err(|e| e.to_string())
        })
    }
}

type Provider = Arc<dyn Fn() -> Result<serde_json::Value, String> + Send + Sync>;
type Providers = Arc<Mutex<HashMap<MakiId, Provider>>>;

/// Backs `maki.session.read` for the sessions a headless driver runs. Each
/// answers only for its own id, so no session can stand in for another, and
/// a closed one leaves nothing behind.
#[derive(Clone, Default)]
pub(crate) struct HeadlessSessions {
    providers: Providers,
    /// The driver runs one session (`maki -p`, sdk mode), so an id-less ask
    /// means it even outside its callbacks. ACP runs many and has no default.
    sole: bool,
}

impl HeadlessSessions {
    pub(crate) fn sole() -> Self {
        Self {
            sole: true,
            ..Self::default()
        }
    }

    pub(crate) fn provider(&self) -> SessionSnapshotFn {
        let Self { providers, sole } = self.clone();
        Box::new(move |id| {
            let registered = lock(&providers);
            let provider = match id {
                Some(want) => want
                    .parse::<MakiId>()
                    .ok()
                    .and_then(|session| registered.get(&session))
                    .ok_or_else(|| not_live(want)),
                None => registered
                    .values()
                    .next()
                    .filter(|_| sole)
                    .ok_or_else(|| NO_CALLING_SESSION_ERR.to_owned()),
            }?
            .clone();
            drop(registered);
            provider()
        })
    }

    /// Answers for `session` until the returned guard drops.
    #[must_use = "dropping the guard unregisters the session at once"]
    pub(crate) fn register(
        &self,
        session: SessionRef,
        snapshot: &HeadlessSnapshot,
        opening: SessionHeader,
        cwd: &str,
        mode: impl Fn() -> &'static str + Send + Sync + 'static,
    ) -> RegisteredSession {
        let id = session.id();
        let provider = snapshot.provider(session, opening, cwd.to_owned(), self.sole, mode);
        lock(&self.providers).insert(id, Arc::clone(&provider));
        RegisteredSession {
            providers: Arc::clone(&self.providers),
            id,
            provider,
        }
    }
}

/// Unregisters on drop, so every way a session ends drops it. A reload that
/// registered again first keeps its own entry.
pub(crate) struct RegisteredSession {
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
    use test_case::test_case;

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

    fn opening(id: SessionRef) -> SessionOpening {
        SessionOpening {
            id,
            model: MODEL.into(),
            cwd: CWD.into(),
            context_window: WINDOW,
            usage: TokenUsage::default(),
            context_size: 0,
            cost: None,
        }
    }

    /// Keep the returned pair alive for as long as the session should be.
    fn publish(id: MakiId, header: SessionHeader) -> (SharedMessages, SharedHeader) {
        let header: SharedHeader = Arc::new(ArcSwap::from_pointee(header));
        let mirror: SharedMessages = Arc::new(ArcSwap::from_pointee(HistorySnapshot::default()));
        publish_live_session(id, &mirror, &header);
        (mirror, header)
    }

    fn blank_header() -> SessionHeader {
        SessionHeader::of(&StoredSession::new("", ""))
    }

    fn register(
        sessions: &HeadlessSessions,
        session: &SessionRef,
        snapshot: &HeadlessSnapshot,
    ) -> RegisteredSession {
        let header = opening(session.clone()).header();
        sessions.register(session.clone(), snapshot, header, CWD, || MODE_PLAN)
    }

    /// A reload registers the same id before the old run's guard drops, and
    /// that late drop must not unregister the new run.
    #[test]
    fn a_stale_guard_leaves_the_reloaded_session_registered() {
        let sessions = HeadlessSessions::default();
        let read = sessions.provider();
        let session = SessionRef::generate();
        let _live = publish(session.id(), blank_header());
        let old = register(&sessions, &session, &HeadlessSnapshot::default());
        let new = register(&sessions, &session, &HeadlessSnapshot::default());

        drop(old);

        assert!(read(Some(session.as_str())).is_ok());
        drop(new);
        assert_eq!(
            read(Some(session.as_str())).unwrap_err(),
            not_live(&session)
        );
    }

    /// The agent republishes the header only after the turn's `Done` is out,
    /// so a `TurnEnd` handler finds it still working on the old usage. The
    /// tally has the finished turn, the header the rest.
    #[test]
    fn a_read_lays_the_tally_over_the_stale_header() {
        let session = SessionRef::generate();
        let _live = publish(
            session.id(),
            SessionHeader {
                title: TITLE.into(),
                fast: true,
                working: true,
                ..SessionHeader::of(&StoredSession::new(MODEL, CWD))
            },
        );
        let snapshot = HeadlessSnapshot::default();
        snapshot.observe(&done(FIRST_TURN_INPUT));
        let sessions = HeadlessSessions::default();
        let _registered = register(&sessions, &session, &snapshot);

        let read = sessions.provider()(Some(session.as_str())).unwrap();

        assert_eq!(read["status"], STATUS_IDLE);
        assert_eq!(read["usage"]["input_tokens"], FIRST_TURN_INPUT);
        assert_eq!(read["context_size"], FIRST_TURN_INPUT);
        assert_eq!(read["title"], TITLE);
        assert_eq!(read["fast"], true);
        assert_eq!(read["cwd"], CWD);
        assert_eq!(read["focused"], false);
        assert_eq!(read["mode"], MODE_PLAN);
        assert_eq!(read["cost"], TURN_COST);
        assert_eq!(read["context_window"], WINDOW);
    }

    /// A new session publishes no model until its first turn picks one.
    #[test]
    fn a_read_before_the_first_turn_names_the_opening_model() {
        let session = SessionRef::generate();
        let _live = publish(session.id(), blank_header());
        let sessions = HeadlessSessions::default();
        let _registered = register(&sessions, &session, &HeadlessSnapshot::default());

        let read = sessions.provider()(Some(session.as_str())).unwrap();

        assert_eq!(read["model"], MODEL);
    }

    /// A connect that fails never publishes a header, and a `TurnError` handler
    /// still reads the session as it opened.
    #[test]
    fn a_read_before_any_header_answers_from_the_opening() {
        let session = SessionRef::generate();
        let sessions = HeadlessSessions::default();
        let _registered = register(&sessions, &session, &HeadlessSnapshot::default());

        let read = sessions.provider()(Some(session.as_str())).unwrap();

        assert_eq!(read["model"], MODEL);
        assert_eq!(read["cwd"], CWD);
        assert_eq!(read["status"], STATUS_IDLE);
    }

    /// A stored session keeps the cwd it started in, the driver's tools run in
    /// its own.
    #[test]
    fn a_read_names_the_driver_cwd_not_the_stored_one() {
        let session = SessionRef::generate();
        let _live = publish(
            session.id(),
            SessionHeader::of(&StoredSession::new(MODEL, "/stored")),
        );
        let sessions = HeadlessSessions::default();
        let _registered = register(&sessions, &session, &HeadlessSnapshot::default());

        let read = sessions.provider()(Some(session.as_str())).unwrap();

        assert_eq!(read["cwd"], CWD);
    }

    /// A resumed session has spent and has a window before its first new turn,
    /// and what it spends next adds to that.
    #[test]
    fn a_resumed_session_reads_its_restored_bill_and_adds_to_it() {
        const RESTORED_COST: f64 = 3.0;
        let session = SessionRef::generate();
        let _live = publish(session.id(), blank_header());
        let snapshot = HeadlessSnapshot::from(&SessionOpening {
            usage: TokenUsage {
                input: u64::from(SECOND_TURN_INPUT),
                ..Default::default()
            },
            cost: Some(RESTORED_COST),
            ..opening(session.clone())
        });
        let sessions = HeadlessSessions::default();
        let _registered = register(&sessions, &session, &snapshot);
        let read = || sessions.provider()(Some(session.as_str())).unwrap();

        assert_eq!(read()["cost"], RESTORED_COST);
        assert_eq!(read()["context_window"], WINDOW);

        snapshot.observe(&done(FIRST_TURN_INPUT));

        assert_eq!(read()["cost"], RESTORED_COST + TURN_COST);
        assert_eq!(
            read()["usage"]["input_tokens"],
            FIRST_TURN_INPUT + SECOND_TURN_INPUT
        );
    }

    /// Lineage and the driver spell a resumed legacy session in hex, and its
    /// base58 form names the same session.
    #[test]
    fn a_legacy_hex_session_answers_to_its_base58_id() {
        let id = MakiId::generate();
        let hex: String = id.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let session: SessionRef = hex.parse().unwrap();
        let _live = publish(id, blank_header());
        let sessions = HeadlessSessions::default();
        let _registered = register(&sessions, &session, &HeadlessSnapshot::default());
        let read = sessions.provider();

        assert_eq!(read(Some(&id.to_string())).unwrap()["id"], hex);
        let stranger = MakiId::generate().to_string();
        assert_eq!(read(Some(&stranger)).unwrap_err(), not_live(&stranger));
    }

    /// `maki -p` and sdk mode answer an id-less ask with their one session.
    /// ACP has no default, so outside a session's callbacks it fails.
    #[test_case(HeadlessSessions::sole(), Ok(true) ; "sole_driver_answers_for_its_session")]
    #[test_case(HeadlessSessions::default(), Err(NO_CALLING_SESSION_ERR) ; "many_sessions_have_no_default")]
    fn an_id_less_read(sessions: HeadlessSessions, expected: Result<bool, &str>) {
        let session = SessionRef::generate();
        let _live = publish(session.id(), blank_header());
        let _registered = register(&sessions, &session, &HeadlessSnapshot::default());

        let focused = sessions.provider()(None).map(|read| read["focused"] == true);

        assert_eq!(focused, expected.map_err(str::to_owned));
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
