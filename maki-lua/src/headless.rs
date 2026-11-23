//! What a headless driver (`maki -p`, sdk mode, ACP) owes the plugin host
//! for each session it runs: what `maki.session` answers with, the host events
//! the session's agent events map to, and running their handlers on the
//! session's behalf. Every driver feeds its sessions through here, so a
//! capability added once reaches all of them.

use maki_agent::Envelope;
use maki_agent::session::Resumed;
use maki_providers::TokenUsage;
use maki_providers::model::Model;
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
    /// `None` on an unpriced model, or when the driver does not price what a
    /// resumed session spent before.
    pub cost: Option<f64>,
}

impl SessionOpening {
    pub fn new(resumed: &Resumed, model: &Model) -> Self {
        Self {
            id: resumed.id.clone(),
            model: model.spec(),
            context_window: model.context_window,
            usage: resumed
                .session
                .as_ref()
                .map(|session| session.token_usage)
                .unwrap_or_default(),
            context_size: resumed.context_size,
            cost: None,
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
