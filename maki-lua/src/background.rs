//! Background tasks of a chat: subagent sessions a plugin opened by chat
//! session id instead of a tool ctx, and tool calls moved off their turn
//! (Ctrl+B, `expire = "background"`). One lives until the first of: its work
//! ends, the Lua task that opened it ends (only under task scope), its chat
//! session ends, its plugin unloads ([`Lifetime::Plugin`] only), or the user
//! or the model stops it.
//! The agent loop never knows about them, so a turn ending or a rewind that
//! respawns the loop leaves them running.

use std::collections::HashMap;
use std::mem;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use maki_agent::cancel::{CancelHandle, CancelToken, CancelTrigger};
use maki_agent::{AgentEvent, AgentSeed, Envelope, SessionMailbox, ToolDoneEvent};
use maki_storage::id::MakiId;
use mlua::Lua;

use crate::api::session::not_live;
use crate::api::util::command::UiAction;

const STOP_REQUESTED: &str = "stop requested";
const FINISHED: &str = "finished";
const FAILED: &str = "failed";
const CANCELLED: &str = "cancelled";
const NONE_RUNNING: &str = "none running";
const TASK_EVENT_RUN_ID: u64 = 0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Agent,
    Call { tool: Arc<str> },
}

/// What ends an entry besides a stop and the end of its chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lifetime {
    /// Ends with its plugin (`/reload`).
    Plugin,
    /// A moved call: survives `/reload`, as an in-flight call does.
    Call,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Stop {
    None,
    User,
    Model,
}

/// Who stopped the task. Shared with whoever must tell a stop from a normal
/// end once the cancel cut its work short.
#[derive(Clone, Default)]
pub(crate) struct StopMark(Arc<AtomicU8>);

impl StopMark {
    pub(crate) fn mark(&self, stop: Stop) {
        self.0.store(stop as u8, Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> Stop {
        match self.0.load(Ordering::Relaxed) {
            stop if stop == Stop::User as u8 => Stop::User,
            stop if stop == Stop::Model as u8 => Stop::Model,
            _ => Stop::None,
        }
    }
}

/// How retiring an entry reaches its work.
pub(crate) enum Fire {
    /// The registry owns the work's trigger: dropping it fires.
    Owned(#[expect(dead_code, reason = "held: dropping it fires the work")] CancelTrigger),
    /// Somebody else owns the trigger (a session stub under a moved call).
    Handle(CancelHandle),
}

impl Fire {
    /// Fires the work now and leaves an inert trigger behind, so the entry can
    /// stay until its final reply is delivered.
    fn disarm(&mut self) -> Self {
        mem::replace(self, Self::Owned(CancelToken::new().0))
    }
}

impl Drop for Fire {
    fn drop(&mut self) {
        if let Self::Handle(handle) = self {
            handle.fire();
        }
    }
}

pub(crate) struct BackgroundTask {
    pub(crate) info: BackgroundInfo,
    pub(crate) session: MakiId,
    /// `None` under session scope: nothing but the chat and the plugin ends it.
    pub(crate) task: Option<u64>,
    pub(crate) lifetime: Lifetime,
    pub(crate) stop: StopMark,
    pub(crate) cancel: Fire,
}

/// What `maki.agent.list()` and `/tasks` show.
#[derive(Clone)]
pub(crate) struct BackgroundInfo {
    pub(crate) id: String,
    pub(crate) plugin: Arc<str>,
    /// A moved call's start summary.
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) running: bool,
    pub(crate) started_at: u64,
    pub(crate) cost: Option<f64>,
}

/// Who a final reply is for, copied out so nothing is held while it is sent.
struct Landing {
    session: MakiId,
    plugin: Arc<str>,
    summary: String,
    tool: Option<Arc<str>>,
    stop: Stop,
}

/// Each session handle keeps a clone, and a handle dropped mid-prompt can
/// close from another thread, hence the `Mutex`.
#[derive(Clone, Default)]
pub(crate) struct BackgroundTasks {
    entries: Arc<Mutex<Vec<BackgroundTask>>>,
    /// The model's `task_stop`, parked until the final reply of the call it
    /// stopped. Whoever removes the sender decides who gets the output.
    waiters: Arc<Mutex<HashMap<String, flume::Sender<String>>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_running_call(entry: &BackgroundTask) -> bool {
    matches!(entry.info.kind, Kind::Call { .. }) && entry.stop.get() == Stop::None
}

fn no_running_task_err(id: &str, running: &[String]) -> String {
    let running = if running.is_empty() {
        NONE_RUNNING.to_owned()
    } else {
        format!("running: {}", running.join(", "))
    };
    format!("no running background task {id}; {running}")
}

impl BackgroundTasks {
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
    pub(crate) fn register(&self, entry: BackgroundTask) -> Result<(), String> {
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

    pub(crate) fn update(&self, id: &str, apply: impl FnOnce(&mut BackgroundTask)) {
        if let Some(entry) = self.locked().iter_mut().find(|e| e.info.id == id) {
            apply(entry);
        }
    }

    pub(crate) fn remove(&self, id: &str) {
        self.retire(|e| e.info.id == id);
    }

    /// Marked before the work is cut short, so it reads the mark. A moved call
    /// stays until [`Self::deliver`], which needs its session, plugin and
    /// mark. Its work is fired now, outside the lock.
    pub(crate) fn stop(&self, id: &str, by: Stop) {
        let fired = {
            let mut entries = self.locked();
            let Some(entry) = entries.iter_mut().find(|e| e.info.id == id) else {
                return;
            };
            entry.stop.mark(by);
            matches!(entry.info.kind, Kind::Call { .. }).then(|| entry.cancel.disarm())
        };
        if fired.is_none() {
            self.remove(id);
        }
    }

    /// The model's stop of one of `session`'s moved calls: waits up to `wait`
    /// for the final reply and returns its text, and then the chat gets no
    /// notice of the end. A late reply returns [`STOP_REQUESTED`] and still
    /// reaches the chat, as a quiet notice.
    pub(crate) async fn stop_by_model(
        &self,
        session: MakiId,
        id: &str,
        wait: Duration,
    ) -> Result<String, String> {
        // Armed before the stop fires, so a call that ignores the stop is
        // abandoned (cancel + the same wait) only after this gives up.
        let timer = smol::Timer::after(wait);
        let reply = self.begin_model_stop(session, id)?;
        let arrived = futures_lite::future::or(async { reply.recv_async().await.ok() }, async {
            timer.await;
            None
        })
        .await;
        if let Some(output) = arrived {
            return Ok(output);
        }
        lock(&self.waiters).remove(id);
        Ok(reply
            .try_recv()
            .unwrap_or_else(|_| STOP_REQUESTED.to_owned()))
    }

    fn begin_model_stop(
        &self,
        session: MakiId,
        id: &str,
    ) -> Result<flume::Receiver<String>, String> {
        let running: Vec<_> = self
            .locked()
            .iter()
            .filter(|e| e.session == session && is_running_call(e))
            .map(|e| e.info.id.clone())
            .collect();
        if !running.iter().any(|running_id| running_id == id) {
            return Err(no_running_task_err(id, &running));
        }
        let (waiter, reply) = flume::bounded(1);
        lock(&self.waiters).insert(id.to_owned(), waiter);
        self.stop(id, Stop::Model);
        Ok(reply)
    }

    /// The final reply of a moved call: tells the chat, closes its task tab
    /// and retires the entry. A model's `task_stop` that is waiting takes the
    /// output instead of the chat getting a notice.
    pub(crate) fn deliver(
        &self,
        id: &str,
        done: ToolDoneEvent,
        ui: Option<flume::Sender<UiAction>>,
    ) {
        let Some(landing) = self.landing(id) else {
            tracing::debug!(id, "final reply of a background task nobody registered");
            return;
        };
        let output = done.output.as_text();
        if !self.hand_to_waiter(id, &output) {
            let tool = landing.tool.as_deref().unwrap_or(&*done.tool);
            Self::notify_chat(&landing, tool, id, done.is_error, &output);
        }
        if let Some(ui) = ui {
            for event in [
                AgentEvent::ToolDone(Box::new(done)),
                AgentEvent::StreamClosed,
            ] {
                let _ = ui.try_send(UiAction::PluginTaskEvent {
                    session: landing.session,
                    plugin: Arc::clone(&landing.plugin),
                    envelope: Box::new(Envelope {
                        event,
                        subagent: None,
                        run_id: TASK_EVENT_RUN_ID,
                    }),
                });
            }
        }
        self.remove(id);
    }

    fn landing(&self, id: &str) -> Option<Landing> {
        self.locked()
            .iter()
            .find(|e| e.info.id == id)
            .map(|e| Landing {
                session: e.session,
                plugin: Arc::clone(&e.info.plugin),
                summary: e.info.name.clone(),
                tool: match &e.info.kind {
                    Kind::Call { tool } => Some(Arc::clone(tool)),
                    Kind::Agent => None,
                },
                stop: e.stop.get(),
            })
    }

    /// Under the lock the timeout path takes to withdraw, so the output goes to
    /// exactly one of the stop and the chat.
    fn hand_to_waiter(&self, id: &str, output: &str) -> bool {
        lock(&self.waiters)
            .remove(id)
            .is_some_and(|waiter| waiter.send(output.to_owned()).is_ok())
    }

    fn notify_chat(landing: &Landing, tool: &str, id: &str, is_error: bool, output: &str) {
        let outcome = if is_error { FAILED } else { FINISHED };
        let stopped = landing.stop != Stop::None;
        let shown_outcome = if stopped { CANCELLED } else { outcome };
        let summary = &landing.summary;
        let text = format!("Background {tool} {id} ({summary}) {outcome}:\n\n{output}");
        let display = format!("Background {tool} {shown_outcome}: {summary}");
        let notified =
            SessionMailbox::notify(landing.session, text, Some(display.as_str()), !stopped);
        if let Err(error) = notified {
            tracing::debug!(id, %error, "background task ended after its chat session");
        }
    }

    pub(crate) fn end_session(&self, session: MakiId) {
        self.retire(|e| e.session == session);
    }

    pub(crate) fn end_task(&self, task: u64) {
        self.retire(|e| e.task == Some(task));
    }

    /// Moved calls outlive `/reload`, as in-flight calls do.
    pub(crate) fn end_plugin(&self, plugin: &str) {
        self.retire(|e| e.lifetime == Lifetime::Plugin && &*e.info.plugin == plugin);
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

    /// What `maki.agent.list()` shows: the sessions plugins opened by chat id.
    pub(crate) fn list_agents(&self, session: MakiId) -> Vec<BackgroundInfo> {
        self.locked()
            .iter()
            .filter(|e| {
                e.session == session && e.info.kind == Kind::Agent && e.lifetime == Lifetime::Plugin
            })
            .map(|e| e.info.clone())
            .collect()
    }

    fn retire(&self, doomed: impl Fn(&BackgroundTask) -> bool) {
        let retired: Vec<_> = self.locked().extract_if(.., |e| doomed(e)).collect();
        if !retired.is_empty() {
            lock(&self.waiters).retain(|id, _| retired.iter().all(|e| e.info.id != *id));
        }
        drop(retired);
    }

    fn locked(&self) -> MutexGuard<'_, Vec<BackgroundTask>> {
        lock(&self.entries)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use maki_agent::ToolOutput;
    use maki_agent::cancel::CancelToken;
    use maki_agent::tools::test_support::stub_seed;
    use test_case::test_case;

    use super::*;
    use crate::runtime::{TaskCell, TaskScope, lock_cell};

    const PLUGIN: &str = "monitor";
    const TOOL: &str = "bash";
    const CALL_ID: &str = "toolu_01";
    const OTHER_CALL_ID: &str = "toolu_02";
    const SUMMARY: &str = "cargo build";
    const OUTPUT: &str = "compiled";
    const NOT_A_TASK_EVENT: &str = "expected a plugin task event";
    const FOREVER: Duration = Duration::from_secs(3600);

    pub(crate) fn live(session: MakiId) -> (SessionMailbox, Arc<AgentSeed>) {
        let mailbox = SessionMailbox::register(session);
        let seed = mailbox.publish_seed(stub_seed(session));
        (mailbox, seed)
    }

    pub(crate) fn entry(session: MakiId, task: Option<u64>) -> (BackgroundTask, CancelToken) {
        let (cancel, token) = CancelToken::new();
        let entry = BackgroundTask {
            info: BackgroundInfo {
                id: MakiId::generate().to_string(),
                plugin: Arc::from(PLUGIN),
                name: String::new(),
                kind: Kind::Agent,
                running: false,
                started_at: 0,
                cost: None,
            },
            session,
            task,
            lifetime: Lifetime::Plugin,
            stop: StopMark::default(),
            cancel: Fire::Owned(cancel),
        };
        (entry, token)
    }

    #[test]
    fn end_session_cancels_its_entries_and_a_respawn_cancels_none() {
        let (ended, respawned) = (MakiId::generate(), MakiId::generate());
        let _ended_live = live(ended);
        let (respawned_box, old_seed) = live(respawned);
        let registry = BackgroundTasks::default();
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
        let registry = BackgroundTasks::of(&lua);
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
        let registry = BackgroundTasks::default();
        let (late, token) = entry(session, Some(1));

        assert_eq!(registry.register(late), Err(not_live(session)));
        assert!(token.is_cancelled());
        assert_eq!(registry.list(session).len(), 0);
    }

    fn call_entry(session: MakiId, id: &str) -> (BackgroundTask, CancelToken) {
        let (mut entry, token) = entry(session, None);
        entry.info.id = id.to_owned();
        entry.info.name = SUMMARY.to_owned();
        entry.info.kind = Kind::Call {
            tool: Arc::from(TOOL),
        };
        entry.lifetime = Lifetime::Call;
        (entry, token)
    }

    fn done(output: &str, is_error: bool) -> ToolDoneEvent {
        ToolDoneEvent {
            id: CALL_ID.to_owned(),
            tool: Arc::from(TOOL),
            output: Arc::new(ToolOutput::Plain(output.into())),
            is_error,
            annotation: None,
            written_path: None,
            call: None,
            backgrounded: false,
        }
    }

    fn registry_with_call(session: MakiId) -> (BackgroundTasks, CancelToken, StopMark) {
        let registry = BackgroundTasks::default();
        let (call, token) = call_entry(session, CALL_ID);
        let mark = call.stop.clone();
        registry.register(call).unwrap();
        (registry, token, mark)
    }

    #[test]
    fn reload_spares_moved_calls_and_session_end_retires_them() {
        let session = MakiId::generate();
        let _live = live(session);
        let registry = BackgroundTasks::default();
        let (call, call_token) = call_entry(session, CALL_ID);
        let (agent, agent_token) = entry(session, None);
        registry.register(call).unwrap();
        registry.register(agent).unwrap();

        registry.end_plugin(PLUGIN);

        assert!(agent_token.is_cancelled());
        assert!(!call_token.is_cancelled());
        assert!(registry.contains(CALL_ID));

        registry.end_session(session);

        assert!(call_token.is_cancelled());
        assert!(!registry.contains(CALL_ID));
    }

    #[test]
    fn stopping_a_moved_call_fires_it_but_keeps_the_entry_until_deliver() {
        let session = MakiId::generate();
        let _live = live(session);
        let (registry, token, mark) = registry_with_call(session);

        registry.stop(CALL_ID, Stop::User);

        assert!(token.is_cancelled());
        assert_eq!(mark.get(), Stop::User);
        assert!(registry.contains(CALL_ID));

        registry.deliver(CALL_ID, done(OUTPUT, true), None);

        assert!(!registry.contains(CALL_ID));
    }

    #[test]
    fn stopping_a_session_entry_removes_it_at_once() {
        let session = MakiId::generate();
        let _live = live(session);
        let registry = BackgroundTasks::default();
        let (opened, token) = entry(session, None);
        let id = opened.info.id.clone();
        registry.register(opened).unwrap();

        registry.stop(&id, Stop::User);

        assert!(token.is_cancelled());
        assert!(!registry.contains(&id));
    }

    #[test]
    fn list_agents_leaves_moved_calls_out() {
        let session = MakiId::generate();
        let _live = live(session);
        let (registry, ..) = registry_with_call(session);
        let (opened, _token) = entry(session, None);
        let opened_id = opened.info.id.clone();
        registry.register(opened).unwrap();

        let ids: Vec<_> = registry
            .list_agents(session)
            .into_iter()
            .map(|info| info.id)
            .collect();

        assert_eq!(ids, [opened_id]);
        assert_eq!(registry.list(session).len(), 2);
    }

    #[test_case(Stop::None, false, FINISHED, FINISHED, true ; "finished_wakes_the_chat")]
    #[test_case(Stop::None, true, FAILED, FAILED, true ; "failed_wakes_the_chat")]
    #[test_case(Stop::User, true, FAILED, CANCELLED, false ; "stopped_by_the_user_is_quiet")]
    fn deliver_tells_the_chat(
        stop: Stop,
        is_error: bool,
        text_outcome: &str,
        display_outcome: &str,
        wakes: bool,
    ) {
        let session = MakiId::generate();
        let (mailbox, _seed) = live(session);
        let (registry, ..) = registry_with_call(session);
        if stop != Stop::None {
            registry.stop(CALL_ID, stop);
        }

        registry.deliver(CALL_ID, done(OUTPUT, is_error), None);

        let woken = mailbox.claim_wake();
        let notices = if wakes {
            woken
        } else {
            assert!(woken.is_empty());
            mailbox.drain()
        };
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].first_text_content(),
            Some(
                format!("Background {TOOL} {CALL_ID} ({SUMMARY}) {text_outcome}:\n\n{OUTPUT}")
                    .as_str()
            )
        );
        assert_eq!(
            notices[0].display_text.as_deref(),
            Some(format!("Background {TOOL} {display_outcome}: {SUMMARY}").as_str())
        );
        assert!(!registry.contains(CALL_ID));
    }

    #[test]
    fn deliver_closes_the_task_tab_with_the_final_reply() {
        let session = MakiId::generate();
        let _live = live(session);
        let (registry, ..) = registry_with_call(session);
        let (tx, rx) = flume::unbounded();

        registry.deliver(CALL_ID, done(OUTPUT, false), Some(tx));

        let events: Vec<_> = rx
            .try_iter()
            .map(|action| {
                let UiAction::PluginTaskEvent {
                    session: chat,
                    plugin,
                    envelope,
                } = action
                else {
                    panic!("{NOT_A_TASK_EVENT}");
                };
                assert_eq!((chat, &*plugin), (session, PLUGIN));
                envelope.event
            })
            .collect();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AgentEvent::ToolDone(done) if done.id == CALL_ID));
        assert!(matches!(events[1], AgentEvent::StreamClosed));
    }

    #[test]
    fn deliver_of_an_unknown_id_does_nothing() {
        let session = MakiId::generate();
        let (mailbox, _seed) = live(session);
        let (registry, ..) = registry_with_call(session);
        let (tx, rx) = flume::unbounded();

        registry.deliver(OTHER_CALL_ID, done(OUTPUT, false), Some(tx));

        assert!(mailbox.drain().is_empty());
        assert!(rx.is_empty());
        assert!(registry.contains(CALL_ID));
    }

    #[test]
    fn model_stop_that_gets_the_output_leaves_the_chat_no_notice() {
        let session = MakiId::generate();
        let (mailbox, _seed) = live(session);
        let (registry, token, mark) = registry_with_call(session);

        let (stopped, ()) = smol::block_on(futures_lite::future::zip(
            registry.stop_by_model(session, CALL_ID, FOREVER),
            async { registry.deliver(CALL_ID, done(OUTPUT, true), None) },
        ));

        assert_eq!(stopped, Ok(OUTPUT.to_owned()));
        assert_eq!(mark.get(), Stop::Model);
        assert!(token.is_cancelled());
        assert!(mailbox.drain().is_empty());
        assert!(!registry.contains(CALL_ID));
    }

    #[test]
    fn model_stop_that_times_out_gets_the_late_output_as_a_quiet_notice() {
        let session = MakiId::generate();
        let (mailbox, _seed) = live(session);
        let (registry, ..) = registry_with_call(session);

        let stopped = smol::block_on(registry.stop_by_model(session, CALL_ID, Duration::ZERO));

        assert_eq!(stopped, Ok(STOP_REQUESTED.to_owned()));
        assert!(registry.contains(CALL_ID));

        registry.deliver(CALL_ID, done(OUTPUT, true), None);

        assert!(mailbox.claim_wake().is_empty());
        let notices = mailbox.drain();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].first_text_content().unwrap().ends_with(OUTPUT));
    }

    #[test]
    fn model_stop_whose_caller_went_away_still_reaches_the_chat() {
        let session = MakiId::generate();
        let (mailbox, _seed) = live(session);
        let (registry, ..) = registry_with_call(session);
        drop(registry.begin_model_stop(session, CALL_ID).unwrap());

        registry.deliver(CALL_ID, done(OUTPUT, false), None);

        assert_eq!(mailbox.drain().len(), 1);
    }

    #[test_case(true, "missing", "running: toolu_01, toolu_02" ; "unknown_id_lists_the_chats_running_tasks")]
    #[test_case(false, CALL_ID, "none running" ; "another_chats_id_is_refused")]
    fn model_stop_refuses_what_the_chat_does_not_run(
        asker_is_owner: bool,
        target: &str,
        listing: &str,
    ) {
        let (owner, stranger) = (MakiId::generate(), MakiId::generate());
        let _live = (live(owner), live(stranger));
        let registry = BackgroundTasks::default();
        let tokens = [CALL_ID, OTHER_CALL_ID].map(|id| {
            let (call, token) = call_entry(owner, id);
            registry.register(call).unwrap();
            token
        });
        let asker = if asker_is_owner { owner } else { stranger };

        let stopped = smol::block_on(registry.stop_by_model(asker, target, FOREVER));

        assert_eq!(
            stopped,
            Err(format!("no running background task {target}; {listing}"))
        );
        assert!(tokens.iter().all(|token| !token.is_cancelled()));
    }
}
