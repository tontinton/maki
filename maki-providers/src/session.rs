use std::sync::Arc;

use maki_storage::id::SessionRef;
use smol::lock::Mutex;

use crate::providers::openai::routing::RoutingState;
use crate::providers::openai::websocket::ResponsesSession;

/// In-memory provider state owned by one conversation, independent of storage identity.
#[derive(Clone)]
pub struct ProviderSession {
    inner: Arc<SessionState>,
}

struct SessionState {
    session_ref: SessionRef,
    thread_id: SessionRef,
    cache_key: String,
    responses: Mutex<ResponsesSession>,
    routing: RoutingState,
}

impl ProviderSession {
    pub fn new(session_ref: SessionRef) -> Self {
        Self::with_identity(
            session_ref.clone(),
            session_ref.clone(),
            session_ref.as_str().into(),
        )
    }

    fn with_identity(session_ref: SessionRef, thread_id: SessionRef, cache_key: String) -> Self {
        Self {
            inner: Arc::new(SessionState {
                session_ref,
                thread_id,
                cache_key,
                responses: Mutex::new(ResponsesSession::default()),
                routing: RoutingState::default(),
            }),
        }
    }

    pub fn child(&self, cache_key: Option<&str>) -> Self {
        Self::with_identity(
            self.inner.session_ref.clone(),
            SessionRef::generate(),
            cache_key.unwrap_or(self.cache_key()).into(),
        )
    }

    pub fn as_str(&self) -> &str {
        self.session_ref().as_str()
    }

    pub fn session_ref(&self) -> &SessionRef {
        &self.inner.session_ref
    }
    pub fn thread_id(&self) -> &str {
        self.inner.thread_id.as_str()
    }
    pub fn cache_key(&self) -> &str {
        &self.inner.cache_key
    }

    pub async fn begin_turn(&self) {
        let _responses = self.inner.responses.lock().await;
        self.inner.routing.clear();
    }

    pub(crate) fn routing(&self) -> &RoutingState {
        &self.inner.routing
    }

    pub(crate) fn responses(&self) -> &Mutex<ResponsesSession> {
        &self.inner.responses
    }
}
