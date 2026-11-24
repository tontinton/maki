//! What a headless driver (`maki -p`, sdk mode, ACP) owes the plugin host
//! for each session it runs: what `maki.session` answers with, the host events
//! the session's agent events map to, and running their handlers on the
//! session's behalf. Every driver feeds its sessions through here, so a
//! capability added once reaches all of them.

use maki_agent::Envelope;
use maki_agent::session::Resumed;
use maki_providers::model::Model;
use maki_providers::{TokenUsage, settle_session};
use maki_storage::id::SessionRef;

use crate::EventHandle;
use crate::agent_autocmd;
use crate::api::session::SessionSnapshotFn;
use crate::session_snapshot::{HeadlessSessions, HeadlessSnapshot, RegisteredSession};

/// One per plugin host, installed as what `maki.session` answers from.
#[derive(Clone)]
pub struct HeadlessHost {
    handle: EventHandle,
    sessions: HeadlessSessions,
}

impl HeadlessHost {
    /// For a driver that runs one session (`maki -p`, sdk mode), so an id-less
    /// ask means it even outside its callbacks.
    pub fn sole(handle: EventHandle) -> Self {
        Self::install(handle, HeadlessSessions::sole())
    }

    /// For ACP, which runs many sessions on one host and has no default one.
    pub fn many(handle: EventHandle) -> Self {
        Self::install(handle, HeadlessSessions::default())
    }

    fn install(handle: EventHandle, sessions: HeadlessSessions) -> Self {
        handle.install_session_snapshot(sessions.provider());
        Self { handle, sessions }
    }

    /// Answers like the one `maki.session.read` asks.
    pub fn provider(&self) -> SessionSnapshotFn {
        self.sessions.provider()
    }

    /// Answers for the session until the returned value drops. `mode` is a
    /// callback because sdk mode and ACP can switch between build and plan
    /// mid-run.
    #[must_use = "dropping it unregisters the session at once"]
    pub fn open(
        &self,
        opening: SessionOpening,
        mode: impl Fn() -> &'static str + Send + Sync + 'static,
    ) -> HostedSession {
        let snapshot = HeadlessSnapshot::opened(&opening);
        let registered = self
            .sessions
            .register(opening.id.clone(), &snapshot, opening.model, mode);
        HostedSession {
            observer: SessionObserver {
                handle: self.handle.clone(),
                session: opening.id,
                snapshot,
            },
            _registered: registered,
        }
    }
}

/// What the plugin host is told about a session as its driver opens it.
pub struct SessionOpening {
    pub id: SessionRef,
    /// Model spec (`provider/id`), until a turn publishes the one it runs.
    pub model: String,
    pub context_window: u32,
    /// What the session spent before this process opened it.
    pub usage: TokenUsage,
    pub context_size: u32,
    /// What that spend billed, `None` on an unpriced model.
    pub cost: Option<f64>,
}

impl SessionOpening {
    /// Prices what a resumed session spent before against `model`, so `usage`
    /// and `cost` start from the same restored session.
    pub fn new(resumed: &Resumed, model: &Model) -> Self {
        let (usage, cost) = resumed.session.as_ref().map_or_else(
            || (TokenUsage::default(), None),
            |session| {
                // A copy: settling fills estimates the run must not save back.
                let mut by_model = session.usage_by_model().clone();
                let cost = settle_session(&session.token_usage, &mut by_model, model, false);
                (session.token_usage, cost)
            },
        );
        Self {
            id: resumed.id.clone(),
            model: model.spec(),
            context_window: model.context_window,
            usage,
            context_size: resumed.context_size,
            cost,
        }
    }
}

/// A session the plugin host answers for.
pub struct HostedSession {
    observer: SessionObserver,
    _registered: RegisteredSession,
}

impl HostedSession {
    /// See [`SessionObserver::observe`].
    pub fn observe(&self, envelope: &Envelope) {
        self.observer.observe(envelope);
    }

    /// For a pump that runs apart from what holds the session open.
    pub fn observer(&self) -> SessionObserver {
        self.observer.clone()
    }
}

#[derive(Clone)]
pub struct SessionObserver {
    handle: EventHandle,
    session: SessionRef,
    snapshot: HeadlessSnapshot,
}

impl SessionObserver {
    /// Hand it every envelope of the session's stream, subagents' included,
    /// in order. Folded in before the host events fire, so a `TurnEnd`
    /// handler reads the finished turn.
    pub fn observe(&self, envelope: &Envelope) {
        self.snapshot.observe(envelope);
        agent_autocmd::dispatch(&self.handle, &self.session, envelope);
    }

    /// Folds {envelope} into what `maki.session.read` answers, firing no host
    /// events.
    pub fn fold(&self, envelope: &Envelope) {
        self.snapshot.observe(envelope);
    }

    #[doc(hidden)]
    pub fn disconnected_for_test(session: SessionRef) -> Self {
        Self {
            handle: EventHandle::disconnected_for_test(),
            session,
            snapshot: HeadlessSnapshot::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use maki_agent::session::StoredSession;
    use maki_storage::sessions::StoredTokenUsage;

    use super::*;

    const SPEC: &str = "anthropic/claude-sonnet-4-5";
    const RECORDED_COST: f64 = 3.0;

    #[test]
    fn a_resumed_opening_starts_from_the_bill_it_recorded() {
        let model = Model::from_spec(SPEC).expect("a shipped model");
        let mut session = StoredSession::new(SPEC, "/work");
        session.token_usage = TokenUsage {
            input: 1_000,
            ..Default::default()
        };
        session.add_model_usage(
            &model.id,
            StoredTokenUsage {
                input: 1_000,
                cost: Some(RECORDED_COST),
                ..Default::default()
            },
        );
        let resumed = Resumed::stored(SessionRef::generate(), session);

        let opening = SessionOpening::new(&resumed, &model);

        assert_eq!(opening.cost, Some(RECORDED_COST));
        assert_eq!(opening.usage.input, 1_000);
    }

    #[test]
    fn a_new_opening_has_spent_nothing() {
        let model = Model::from_spec(SPEC).expect("a shipped model");
        let resumed = Resumed::empty(SessionRef::generate());

        let opening = SessionOpening::new(&resumed, &model);

        assert_eq!(opening.cost, None);
        assert_eq!(opening.usage, TokenUsage::default());
    }
}
