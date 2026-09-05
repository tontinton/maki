use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use maki_agent::Envelope;

/// One push frame: a mirrored agent envelope, a synthetic status frame, or a
/// permission state change.
#[derive(Debug, Clone)]
pub enum RemoteUpdate {
    Envelope {
        session: String,
        event: serde_json::Value,
    },
    Status {
        session: String,
        status: &'static str,
    },
    Permission {
        session: String,
        frame: PermissionFrame,
    },
    /// The pending permission was answered (locally or remotely), so no
    /// remote client should keep showing the prompt.
    PermissionResolved {
        session: String,
        request_id: String,
    },
    Shutdown,
}

impl RemoteUpdate {
    /// The session this update belongs to; `Shutdown` fans out to everyone.
    pub fn session(&self) -> Option<&str> {
        match self {
            Self::Envelope { session, .. }
            | Self::Status { session, .. }
            | Self::Permission { session, .. }
            | Self::PermissionResolved { session, .. } => Some(session),
            Self::Shutdown => None,
        }
    }
}

/// What a remote client needs to render one approval card.
#[derive(Debug, Clone)]
pub struct PermissionFrame {
    pub id: String,
    pub tool: String,
    pub scopes: Vec<String>,
}

/// A live SSE subscriber. Unregisters from the fan-out when dropped, so every
/// stream end (normal, error, panic unwinding) cleans up.
#[derive(Debug)]
pub struct Subscription {
    id: u64,
    pub updates: flume::Receiver<RemoteUpdate>,
    inner: Arc<Inner>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.inner.updates.rcu(|subs| {
            subs.iter()
                .filter(|sub| sub.id != self.id)
                .cloned()
                .collect::<Vec<_>>()
        });
    }
}

/// One attached browser. `session` is the tab the stream was scoped to;
/// `None` is an unscoped viewer that can flip to any tab, so it counts as
/// watching whichever tab the TUI asks about.
#[derive(Debug, Clone)]
struct Subscriber {
    id: u64,
    tx: flume::Sender<RemoteUpdate>,
    session: Option<String>,
}

/// Fan-out hub between the event loop's tee and every connected web client.
///
/// Envelopes are serialized to JSON once, here, not per subscriber. Slow
/// subscribers drop frames rather than stall the agent: each subscriber holds
/// its own bounded queue and `send` gives up on a full one.
#[derive(Debug, Clone)]
pub struct RemoteState {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    updates: ArcSwap<Vec<Subscriber>>,
    next_id: AtomicU64,
}

impl Default for RemoteState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                updates: ArcSwap::from_pointee(Vec::new()),
                next_id: AtomicU64::new(1),
            }),
        }
    }
}

const SUBSCRIBER_QUEUE_CAP: usize = 512;

impl RemoteState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches a browser stream. `session` is the tab the stream is scoped
    /// to (None for the unscoped index, which can flip tabs at any time).
    pub fn subscribe(&self, session: Option<String>) -> Subscription {
        let (tx, updates) = flume::bounded(SUBSCRIBER_QUEUE_CAP);
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.updates.rcu(|subs| {
            let mut subs = Vec::clone(subs);
            subs.push(Subscriber {
                id,
                tx: tx.clone(),
                session: session.clone(),
            });
            subs
        });
        Subscription {
            id,
            updates,
            inner: Arc::clone(&self.inner),
        }
    }

    /// Browsers attached right now, counting an unscoped viewer toward any
    /// tab it could switch to. The TUI asks for the focused tab.
    pub fn viewers(&self, focused: &str) -> usize {
        self.inner
            .updates
            .load()
            .iter()
            .filter(|sub| sub.session.as_deref().is_none_or(|s| s == focused))
            .count()
    }

    /// Who is attached right now, grouped by tab. `None` keys the unscoped
    /// viewers that can flip tabs at will.
    pub fn viewers_by_session(&self) -> Vec<(Option<String>, usize)> {
        let subs = self.inner.updates.load();
        let mut grouped: Vec<(Option<String>, usize)> = Vec::new();
        for sub in subs.iter() {
            match grouped.iter_mut().find(|(s, _)| *s == sub.session) {
                Some((_, n)) => *n += 1,
                None => grouped.push((sub.session.clone(), 1)),
            }
        }
        grouped
    }

    /// Any browser attached at all, on any tab.
    pub fn has_viewers(&self) -> bool {
        !self.inner.updates.load().is_empty()
    }

    fn publish(&self, update: RemoteUpdate) {
        let subs = self.inner.updates.load_full();
        for sub in subs.iter() {
            let _ = sub.tx.try_send(update.clone());
        }
    }

    /// Serializes once and mirrors to all clients. Called for every envelope
    /// the TUI event loop receives while remote control is up.
    pub fn send_envelope(&self, session_id: &str, envelope: &Envelope) {
        let Ok(event) = serde_json::to_value(envelope) else {
            return;
        };
        self.publish(RemoteUpdate::Envelope {
            session: session_id.to_owned(),
            event,
        });
    }

    pub fn send_status(&self, session_id: &str, status: &'static str) {
        self.publish(RemoteUpdate::Status {
            session: session_id.to_owned(),
            status,
        });
    }

    pub fn send_permission(&self, session_id: &str, frame: PermissionFrame) {
        self.publish(RemoteUpdate::Permission {
            session: session_id.to_owned(),
            frame,
        });
    }

    pub fn send_permission_resolved(&self, session_id: &str, request_id: &str) {
        self.publish(RemoteUpdate::PermissionResolved {
            session: session_id.to_owned(),
            request_id: request_id.to_owned(),
        });
    }

    pub fn send_shutdown(&self) {
        self.publish(RemoteUpdate::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "s1";
    const STATUS_IDLE: &str = "idle";

    #[test]
    fn subscribers_receive_independently_and_slow_one_drops() {
        let state = RemoteState::new();
        let a = state.subscribe(None);
        let b = state.subscribe(None);

        state.send_status(SESSION, STATUS_IDLE);
        assert!(matches!(
            a.updates.try_recv(),
            Ok(RemoteUpdate::Status { status, .. }) if status == STATUS_IDLE
        ));
        assert!(b.updates.try_recv().is_ok());

        // A full queue drops new frames instead of blocking the publisher.
        for _ in 0..SUBSCRIBER_QUEUE_CAP {
            state.send_status(SESSION, STATUS_IDLE);
        }
        state.send_status(SESSION, STATUS_IDLE);
        let mut received = 0;
        while a.updates.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, SUBSCRIBER_QUEUE_CAP, "oldest frames are kept");
        assert!(b.updates.try_recv().is_ok(), "other subscriber unaffected");
    }

    #[test]
    fn viewers_count_scoped_and_unscoped_streams() {
        let state = RemoteState::new();
        assert_eq!(state.viewers("t1"), 0);
        assert!(!state.has_viewers());
        let scoped = state.subscribe(Some("t1".into()));
        let other = state.subscribe(Some("t2".into()));
        let unscoped = state.subscribe(None);
        assert_eq!(state.viewers("t1"), 2, "own tab plus the tab-hopper");
        assert_eq!(state.viewers("t2"), 2);
        assert_eq!(state.viewers("t3"), 1, "only the unscoped viewer");
        drop(scoped);
        drop(unscoped);
        assert_eq!(state.viewers("t1"), 0, "t2's viewer cannot see t1");
        assert!(state.has_viewers());
        drop(other);
        assert!(!state.has_viewers());
    }

    #[test]
    fn dropping_a_subscription_removes_only_that_receiver() {
        let state = RemoteState::new();
        let a = state.subscribe(None);
        let b = state.subscribe(None);
        drop(a);

        state.send_status(SESSION, STATUS_IDLE);
        assert!(b.updates.try_recv().is_ok());
    }
}
