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

/// What a remote client needs to render one approval card.
#[derive(Debug, Clone)]
pub struct PermissionFrame {
    pub id: String,
    pub tool: String,
    pub scopes: Vec<String>,
}

/// A live SSE subscriber. Returned by [`RemoteState::subscribe`] and handed
/// back to [`RemoteState::unsubscribe`] when the stream ends.
#[derive(Debug)]
pub struct Subscription {
    id: u64,
    pub updates: flume::Receiver<RemoteUpdate>,
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
    updates: ArcSwap<Vec<(u64, flume::Sender<RemoteUpdate>)>>,
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

    pub fn subscribe(&self) -> Subscription {
        let (tx, updates) = flume::bounded(SUBSCRIBER_QUEUE_CAP);
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.updates.rcu(|subs| {
            let mut subs = Vec::clone(subs);
            subs.push((id, tx.clone()));
            subs
        });
        Subscription { id, updates }
    }

    /// Removes a subscriber's queue slot; used when its SSE stream ends.
    pub fn unsubscribe(&self, sub: &Subscription) {
        self.inner.updates.rcu(|subs| {
            subs.iter()
                .filter(|(id, _)| *id != sub.id)
                .cloned()
                .collect::<Vec<_>>()
        });
    }

    fn publish(&self, update: RemoteUpdate) {
        let subs = self.inner.updates.load_full();
        for (_, tx) in subs.iter() {
            let _ = tx.try_send(update.clone());
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
        let a = state.subscribe();
        let b = state.subscribe();

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
    fn unsubscribe_removes_only_that_receiver() {
        let state = RemoteState::new();
        let a = state.subscribe();
        let b = state.subscribe();
        state.unsubscribe(&a);

        state.send_status(SESSION, STATUS_IDLE);
        assert!(b.updates.try_recv().is_ok());
        assert!(a.updates.try_recv().is_err());
    }
}
