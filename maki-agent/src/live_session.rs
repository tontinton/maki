use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};

use arc_swap::ArcSwap;
use maki_config::ModelPolicy;
use maki_providers::{Message, RequestOptions, Timeouts};
use maki_storage::id::{MakiId, SessionRef};
use thiserror::Error;

use crate::cancel::CancelToken;
use crate::mcp::McpSession;
use crate::permissions::PermissionManager;
use crate::tools::{FileAccess, ToolContext, ToolRegistry, interpreter_ctx};
use crate::{AgentConfig, AgentMode, EventSender, ModelSlot, ToolOutputLines};

const MAILBOX_CAPACITY: usize = 100;
/// Nobody listens on an unattended context's events, so its run id is never read.
const UNATTENDED_RUN_ID: u64 = 0;

static LIVE_SESSIONS: LazyLock<Mutex<HashMap<MakiId, Weak<LiveSession>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Every frontend registers the mailbox, but only an interactive agent loop
/// publishes a seed. The seed is held weakly, so it dies with its loop. The
/// mailbox lives on through a respawn, because the new loop registers before
/// the old one lets go.
#[derive(Default)]
struct LiveSession {
    mailbox: Mutex<Mailbox>,
    seed: Mutex<Weak<AgentSeed>>,
}

#[derive(Default)]
struct Mailbox {
    pending: VecDeque<Message>,
    wake: bool,
}

#[derive(Debug, Error)]
#[error("session not live: {0}")]
pub struct MailboxError(MakiId);

#[derive(Clone)]
pub struct SessionMailbox {
    session_id: MakiId,
    live: Arc<LiveSession>,
}

/// A plugin that starts a subagent outside of any tool call has no context
/// to clone, so it builds one from this.
pub struct AgentSeed {
    pub session_id: SessionRef,
    pub model_slot: Arc<ArcSwap<ModelSlot>>,
    pub config: AgentConfig,
    pub tool_output_lines: ToolOutputLines,
    pub permissions: Arc<PermissionManager>,
    pub mcp: Option<McpSession>,
    pub timeouts: Timeouts,
    pub file_access: Arc<FileAccess>,
    pub model_policy: Arc<ModelPolicy>,
    /// Copied from the latest run, so background work never thinks harder
    /// than the user asked for.
    opts: Mutex<RequestOptions>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SessionMailbox {
    pub fn register(session_id: MakiId) -> Self {
        let mut sessions = lock(&LIVE_SESSIONS);
        if let Some(live) = sessions.get(&session_id).and_then(Weak::upgrade) {
            return Self { session_id, live };
        }

        let live = Arc::new(LiveSession::default());
        sessions.insert(session_id, Arc::downgrade(&live));
        Self { session_id, live }
    }

    pub fn notify(session_id: MakiId, text: String, wake: bool) -> Result<(), MailboxError> {
        let live = {
            let mut sessions = lock(&LIVE_SESSIONS);
            let Some(live) = sessions.get(&session_id).and_then(Weak::upgrade) else {
                sessions.remove(&session_id);
                return Err(MailboxError(session_id));
            };
            live
        };
        let mut mailbox = lock(&live.mailbox);
        if mailbox.pending.len() == MAILBOX_CAPACITY {
            mailbox.pending.pop_front();
        }
        mailbox.pending.push_back(Message::observation(text));
        mailbox.wake |= wake;
        Ok(())
    }

    pub fn drain(&self) -> Vec<Message> {
        let mut mailbox = lock(&self.live.mailbox);
        mailbox.wake = false;
        mailbox.pending.drain(..).collect()
    }

    pub fn claim_wake(&self) -> Vec<Message> {
        let mut mailbox = lock(&self.live.mailbox);
        if !mailbox.wake {
            return Vec::new();
        }
        mailbox.wake = false;
        mailbox.pending.drain(..).collect()
    }

    /// The registry only keeps a weak link, so the caller has to hold on to
    /// the seed. After a rewind the new loop starts with the old options, so
    /// background work keeps the effort the user picked.
    pub fn publish_seed(&self, seed: AgentSeed) -> Arc<AgentSeed> {
        let mut published = lock(&self.live.seed);
        if let Some(previous) = published.upgrade() {
            seed.set_opts(previous.opts());
        }
        let seed = Arc::new(seed);
        *published = Arc::downgrade(&seed);
        seed
    }
}

impl Drop for SessionMailbox {
    fn drop(&mut self) {
        if Arc::strong_count(&self.live) != 1 {
            return;
        }
        let weak = Arc::downgrade(&self.live);
        let mut sessions = lock(&LIVE_SESSIONS);
        if Arc::strong_count(&self.live) == 1
            && sessions
                .get(&self.session_id)
                .is_some_and(|registered| registered.ptr_eq(&weak))
        {
            sessions.remove(&self.session_id);
        }
    }
}

impl AgentSeed {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: SessionRef,
        model_slot: Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        permissions: Arc<PermissionManager>,
        mcp: Option<McpSession>,
        timeouts: Timeouts,
        model_policy: Arc<ModelPolicy>,
    ) -> Self {
        Self {
            session_id,
            model_slot,
            config,
            tool_output_lines,
            permissions,
            mcp,
            timeouts,
            file_access: FileAccess::fresh(),
            model_policy,
            opts: Mutex::default(),
        }
    }

    /// `None` when the session is closed or has no interactive loop, like a
    /// headless or ACP run.
    pub fn live(session_id: MakiId) -> Option<Arc<Self>> {
        let live = lock(&LIVE_SESSIONS).get(&session_id)?.upgrade()?;
        lock(&live.seed).upgrade()
    }

    pub fn set_opts(&self, opts: RequestOptions) {
        *lock(&self.opts) = opts;
    }

    fn opts(&self) -> RequestOptions {
        *lock(&self.opts)
    }

    /// No chat shows this work, so its events go to a channel nobody reads.
    pub fn tool_context(&self, cancel: CancelToken) -> ToolContext {
        let (sink, _) = flume::unbounded();
        let slot = self.model_slot.load();
        ToolContext {
            provider: Arc::clone(&slot.provider),
            model: Arc::new(slot.model.clone()),
            session_id: Some(self.session_id.clone()),
            mcp: self.mcp.clone(),
            config: self.config.clone(),
            tool_output_lines: self.tool_output_lines,
            timeouts: self.timeouts,
            opts: self.opts(),
            model_policy: Arc::clone(&self.model_policy),
            ..interpreter_ctx(
                &AgentMode::default(),
                &EventSender::new(sink, UNATTENDED_RUN_ID),
                cancel,
                Arc::clone(&self.permissions),
                self.file_access.sharing_locks(),
                None,
                Arc::clone(ToolRegistry::global_arc()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use maki_providers::ThinkingConfig;

    use super::*;
    use crate::tools::test_support::stub_ctx;

    const LATEST_OPTS: RequestOptions = RequestOptions {
        thinking: ThinkingConfig::Adaptive,
        fast: true,
    };

    fn seed(id: MakiId) -> AgentSeed {
        let ctx = stub_ctx(&AgentMode::Build);
        let slot = ModelSlot {
            model: (*ctx.model).clone(),
            provider: ctx.provider,
        };
        AgentSeed::new(
            SessionRef::from(id),
            Arc::new(ArcSwap::from_pointee(slot)),
            AgentConfig::default(),
            ToolOutputLines::default(),
            ctx.permissions,
            None,
            Timeouts::default(),
            Arc::default(),
        )
    }

    fn text(message: &Message) -> &str {
        message.user_text().unwrap()
    }

    #[test]
    fn notifications_drain_in_order_and_clear_wake() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, "first".into(), true).unwrap();
        SessionMailbox::notify(id, "second".into(), true).unwrap();

        let messages = mailbox.drain();
        assert_eq!(
            messages.iter().map(text).collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(messages.iter().all(Message::is_observation));
        assert!(mailbox.claim_wake().is_empty());
    }

    #[test]
    fn quiet_notifications_do_not_claim_a_wake() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, "built".into(), false).unwrap();

        assert!(mailbox.claim_wake().is_empty());
        assert_eq!(mailbox.drain().len(), 1);
    }

    #[test]
    fn waking_notification_claims_all_pending_messages() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, "quiet".into(), false).unwrap();
        SessionMailbox::notify(id, "wake".into(), true).unwrap();

        let messages = mailbox.claim_wake();
        assert_eq!(
            messages.iter().map(text).collect::<Vec<_>>(),
            ["quiet", "wake"]
        );
        assert!(mailbox.drain().is_empty());
    }

    #[test]
    fn notifications_drop_the_oldest_message_at_capacity() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        for index in 0..=MAILBOX_CAPACITY {
            SessionMailbox::notify(id, index.to_string(), false).unwrap();
        }

        let messages = mailbox.drain();
        assert_eq!(messages.len(), MAILBOX_CAPACITY);
        assert_eq!(text(&messages[0]), "1");
        assert_eq!(text(messages.last().unwrap()), MAILBOX_CAPACITY.to_string());
    }

    #[test]
    fn registrations_for_the_same_id_share_state() {
        let id = MakiId::generate();
        let first = SessionMailbox::register(id);
        let second = SessionMailbox::register(id);
        SessionMailbox::notify(id, "built".into(), false).unwrap();

        assert_eq!(second.drain().len(), 1);
        assert!(first.drain().is_empty());
    }

    #[test]
    fn dropping_the_last_registration_closes_the_mailbox() {
        let id = MakiId::generate();
        drop(SessionMailbox::register(id));

        assert!(!lock(&LIVE_SESSIONS).contains_key(&id));
        assert!(SessionMailbox::notify(id, "late".into(), false).is_err());
    }

    #[test]
    fn dropping_one_registration_keeps_the_shared_mailbox() {
        let id = MakiId::generate();
        let first = SessionMailbox::register(id);
        let second = SessionMailbox::register(id);

        drop(first);

        assert!(lock(&LIVE_SESSIONS).contains_key(&id));
        SessionMailbox::notify(id, "built".into(), false).unwrap();
        assert_eq!(second.drain().len(), 1);
    }

    #[test]
    fn stale_drop_does_not_remove_a_replacement() {
        let id = MakiId::generate();
        let stale = SessionMailbox::register(id);
        let replacement = SessionMailbox {
            session_id: id,
            live: Arc::default(),
        };
        lock(&LIVE_SESSIONS).insert(id, Arc::downgrade(&replacement.live));

        drop(stale);
        SessionMailbox::notify(id, "built".into(), false).unwrap();

        assert_eq!(replacement.drain().len(), 1);
    }

    #[test]
    fn legacy_and_canonical_ids_address_the_same_mailbox() {
        let legacy: MakiId = "01965087-4c71-7f00-8000-000000000001".parse().unwrap();
        let canonical: MakiId = legacy.to_string().parse().unwrap();
        let mailbox = SessionMailbox::register(legacy);
        SessionMailbox::notify(canonical, "built".into(), false).unwrap();

        assert_eq!(mailbox.drain().len(), 1);
    }

    #[test]
    fn seed_is_live_only_while_its_owner_holds_it() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        assert!(AgentSeed::live(id).is_none());

        let owned = mailbox.publish_seed(seed(id));
        assert!(Arc::ptr_eq(&AgentSeed::live(id).unwrap(), &owned));

        drop(owned);
        assert!(AgentSeed::live(id).is_none());
        SessionMailbox::notify(id, "built".into(), true).unwrap();
        assert_eq!(mailbox.claim_wake().len(), 1);
    }

    #[test]
    fn respawned_seed_wins_and_its_tool_context_keeps_the_latest_opts() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let old = mailbox.publish_seed(seed(id));
        old.set_opts(LATEST_OPTS);

        let new = mailbox.publish_seed(seed(id));
        drop(old);

        let live = AgentSeed::live(id).unwrap();
        assert!(Arc::ptr_eq(&live, &new));
        assert_eq!(live.tool_context(CancelToken::none()).opts, LATEST_OPTS);
    }
}
