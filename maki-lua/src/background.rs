//! Subagent sessions a plugin opened by chat session id instead of a tool
//! ctx. One lives until the first of: the Lua task that opened it ends (only
//! under task scope), its chat session ends, its plugin unloads, Esc in its
//! task's tab stops it, or it is closed.
//! The agent loop never knows about them, so a turn ending or a rewind that
//! respawns the loop leaves them running.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use maki_agent::AgentSeed;
use maki_agent::cancel::CancelTrigger;
use maki_storage::id::MakiId;
use mlua::Lua;

use crate::api::session::not_live;

pub(crate) struct BackgroundSession {
    pub(crate) info: BackgroundInfo,
    pub(crate) session: MakiId,
    /// `None` under session scope: nothing but the chat and the plugin ends it.
    pub(crate) task: Option<u64>,
    /// Shared with the session handle, so a `:prompt()` cut short can tell
    /// Esc in the task's tab from the end of its task.
    pub(crate) stopped: Arc<AtomicBool>,
    /// The subagent's cancel parent. Dropping the entry fires it.
    pub(crate) _cancel: CancelTrigger,
}

/// What `maki.agent.list()` shows.
#[derive(Clone)]
pub(crate) struct BackgroundInfo {
    pub(crate) id: String,
    pub(crate) plugin: Arc<str>,
    pub(crate) name: String,
    pub(crate) running: bool,
    pub(crate) started_at: u64,
    pub(crate) cost: Option<f64>,
}

/// Each session handle keeps a clone, and a handle dropped mid-prompt can
/// close from another thread, hence the `Mutex`.
#[derive(Clone, Default)]
pub(crate) struct BackgroundSessions(Arc<Mutex<Vec<BackgroundSession>>>);

impl BackgroundSessions {
    pub(crate) fn of(lua: &Lua) -> Self {
        if let Some(registry) = lua.app_data_ref::<Self>() {
            return registry.clone();
        }
        let registry = Self::default();
        lua.set_app_data(registry.clone());
        registry
    }

    /// The caller found the seed a while ago, and the chat session may have
    /// ended since, back when `end_session` had nothing of ours to cancel. So
    /// we look again under the lock `end_session` takes, or the entry would
    /// outlive its chat session.
    pub(crate) fn register(&self, entry: BackgroundSession) -> Result<(), String> {
        let mut entries = self.locked();
        if AgentSeed::live(entry.session).is_none() {
            return Err(not_live(entry.session));
        }
        entries.push(entry);
        Ok(())
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.locked().iter().any(|entry| entry.info.id == id)
    }

    pub(crate) fn update(&self, id: &str, apply: impl FnOnce(&mut BackgroundSession)) {
        if let Some(entry) = self.locked().iter_mut().find(|e| e.info.id == id) {
            apply(entry);
        }
    }

    pub(crate) fn remove(&self, id: &str) {
        self.retire(|e| e.info.id == id);
    }

    /// Esc in the task's tab. Marked before it is dropped, so the prompt the
    /// cancel cuts short reads the mark.
    pub(crate) fn stop(&self, id: &str) {
        self.update(id, |e| e.stopped.store(true, Ordering::Relaxed));
        self.remove(id);
    }

    pub(crate) fn end_session(&self, session: MakiId) {
        self.retire(|e| e.session == session);
    }

    pub(crate) fn end_task(&self, task: u64) {
        self.retire(|e| e.task == Some(task));
    }

    pub(crate) fn end_plugin(&self, plugin: &str) {
        self.retire(|e| &*e.info.plugin == plugin);
    }

    /// Hands out a copy, because building Lua tables can run the GC, and a
    /// session handle it collects closes through this same lock.
    pub(crate) fn list(&self, session: MakiId) -> Vec<BackgroundInfo> {
        self.locked()
            .iter()
            .filter(|e| e.session == session)
            .map(|e| e.info.clone())
            .collect()
    }

    fn retire(&self, doomed: impl Fn(&BackgroundSession) -> bool) {
        let retired: Vec<_> = self.locked().extract_if(.., |e| doomed(e)).collect();
        drop(retired);
    }

    fn locked(&self) -> MutexGuard<'_, Vec<BackgroundSession>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use maki_agent::SessionMailbox;
    use maki_agent::cancel::CancelToken;
    use maki_agent::tools::test_support::stub_seed;

    use super::*;
    use crate::runtime::{TaskCell, TaskScope, lock_cell};

    const PLUGIN: &str = "monitor";

    fn live(session: MakiId) -> (SessionMailbox, Arc<AgentSeed>) {
        let mailbox = SessionMailbox::register(session);
        let seed = mailbox.publish_seed(stub_seed(session));
        (mailbox, seed)
    }

    fn entry(session: MakiId, task: Option<u64>) -> (BackgroundSession, CancelToken) {
        let (cancel, token) = CancelToken::new();
        let entry = BackgroundSession {
            info: BackgroundInfo {
                id: MakiId::generate().to_string(),
                plugin: Arc::from(PLUGIN),
                name: String::new(),
                running: false,
                started_at: 0,
                cost: None,
            },
            session,
            task,
            stopped: Arc::default(),
            _cancel: cancel,
        };
        (entry, token)
    }

    #[test]
    fn end_session_cancels_its_entries_and_a_respawn_cancels_none() {
        let (ended, respawned) = (MakiId::generate(), MakiId::generate());
        let _ended_live = live(ended);
        let (respawned_box, old_seed) = live(respawned);
        let registry = BackgroundSessions::default();
        let (ended_entry, ended_token) = entry(ended, Some(1));
        let (kept_entry, kept_token) = entry(respawned, Some(1));
        registry.register(ended_entry).unwrap();
        registry.register(kept_entry).unwrap();

        let _new_seed = respawned_box.publish_seed(stub_seed(respawned));
        drop(old_seed);
        registry.end_session(ended);

        assert!(ended_token.is_cancelled());
        assert_eq!(registry.list(ended).len(), 0);
        assert!(!kept_token.is_cancelled());
        assert_eq!(registry.list(respawned).len(), 1);
    }

    #[test]
    fn task_end_retires_task_scope_and_plugin_unload_retires_the_rest() {
        let lua = Lua::new();
        let registry = BackgroundSessions::of(&lua);
        let session = MakiId::generate();
        let _live = live(session);
        let scope = TaskScope::new(&lua, TaskCell::new(CancelToken::none(), None, None));
        let task = lock_cell(scope.handle()).id;
        let (opened, token) = entry(session, Some(task));
        let (kept, kept_token) = entry(session, None);
        registry.register(opened).unwrap();
        registry.register(kept).unwrap();

        drop(scope);

        assert!(token.is_cancelled());
        assert!(!kept_token.is_cancelled());
        assert_eq!(registry.list(session).len(), 1);

        registry.end_plugin(PLUGIN);

        assert!(kept_token.is_cancelled());
        assert_eq!(registry.list(session).len(), 0);
    }

    #[test]
    fn register_after_teardown_fails_and_leaves_nothing() {
        let session = MakiId::generate();
        drop(live(session));
        let registry = BackgroundSessions::default();
        let (late, token) = entry(session, Some(1));

        assert_eq!(registry.register(late), Err(not_live(session)));
        assert!(token.is_cancelled());
        assert_eq!(registry.list(session).len(), 0);
    }
}
