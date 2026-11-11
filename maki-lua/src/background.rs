//! Background tasks of a chat: subagent sessions a plugin opened by chat
//! session id instead of a tool ctx, and tool calls moved off their turn
//! (Ctrl+B, `expire = "background"`). One lives until the first of: its work
//! ends, the Lua task that opened it ends (only under task scope), its chat
//! session ends, its plugin unloads ([`Lifetime::Plugin`] only), the run it
//! reports to ends, or the user or the model stops it.
//! The agent loop never knows about them, so a turn ending or a rewind that
//! respawns the loop leaves them running.

use std::collections::HashMap;
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use maki_agent::cancel::{CancelHandle, CancelToken, CancelTrigger};
use maki_agent::{
    Activity, AgentEvent, AgentSeed, BackgroundNotice, Cutter, EndReason, Envelope, MovedTrigger,
    SessionControl, SessionMailbox, TaskEnd, TaskLabel, ToolDoneEvent, result_notice,
};
use maki_storage::id::MakiId;
use mlua::Lua;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout_at};

use crate::api::session::not_live;
use crate::api::util::command::UiAction;

pub(crate) const NONE_RUNNING: &str = "none running";
pub(crate) const NO_RUNNING_TASK: &str = "no running background task";
const RUNNING_LIST: &str = "running:";
pub(crate) const RUN_ENDED: &str = "the run this call reports to has ended";
const TASK_EVENT_RUN_ID: u64 = 0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Agent,
    Call { tool: Arc<str> },
}

/// What ends an entry besides a stop and the end of its chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lifetime {
    /// Ends when its plugin is unloaded.
    Plugin,
    /// A moved call: survives its plugin's unload, as an in-flight call does.
    Call,
}

/// How retiring an entry reaches its work.
pub(crate) enum Fire {
    /// The registry owns the work's trigger: dropping it fires.
    Owned(#[expect(dead_code, reason = "held: dropping it fires the work")] CancelTrigger),
    /// A moved call: dropping it is a stop, unless [`Self::finish`] ran first.
    Call(MovedTrigger),
    /// Somebody else owns the trigger (a session stub under a moved call).
    Handle(CancelHandle),
}

impl Fire {
    /// Hands back the live trigger, which fires the work when the caller drops
    /// it outside the lock. An inert one stays behind, so the entry can wait
    /// for its final reply.
    fn disarm(&mut self) -> Self {
        mem::replace(self, Self::Owned(CancelToken::new().0))
    }

    fn finish(&self) {
        if let Self::Call(trigger) = self {
            trigger.finish();
        }
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
    /// Who stopped it. Only [`Self::stop`] writes it, the first stop sticks,
    /// and it is written before the cancel fires, so the work cut short can
    /// tell who did it.
    pub(crate) stopped: Option<Cutter>,
    /// The subagent session it runs. A session's own entry has it from the
    /// start. A moved `task` call gets it when the session it runs under the
    /// call's id attaches.
    pub(crate) control: Option<Arc<SessionControl>>,
    /// A moved call whose final reply exists: only dispatch's output layers
    /// stand between it and [`BackgroundTasks::deliver`].
    pub(crate) replied: bool,
    pub(crate) cancel: Fire,
    /// Where a moved call reports: the mailbox of the run that made it, kept
    /// open while the work is out, so the run's receipt survives an agent
    /// respawn.
    pub(crate) mailbox: Option<SessionMailbox>,
}

impl BackgroundTask {
    fn is_call(&self) -> bool {
        matches!(self.info.kind, Kind::Call { .. })
    }

    fn has_final(&self) -> bool {
        self.is_call() && self.replied
    }

    fn stop(&mut self, by: Cutter) {
        let by = *self.stopped.get_or_insert(by);
        if let Some(control) = &self.control {
            control.end(EndReason::Stopped(by));
        }
    }

    /// The session a moved call runs under its own id, opened before or after
    /// the move. A stop that came first ends it too.
    pub(crate) fn attach(&mut self, control: Arc<SessionControl>) {
        if let Some(by) = self.stopped {
            control.end(EndReason::Stopped(by));
        }
        self.control = Some(control);
    }

    fn label(&self, tool: &str) -> TaskLabel {
        TaskLabel::new(tool, Some(&self.info.name))
    }

    /// Only a moved call holds a receipt, so a session entry has nothing to
    /// report.
    fn report_lost(&self, mailbox: &SessionMailbox) {
        if let Kind::Call { tool } = &self.info.kind {
            mailbox.lose(&self.info.id, &self.label(tool));
        }
    }

    fn reports_to(&self, mailbox: MakiId) -> bool {
        self.mailbox
            .as_ref()
            .is_some_and(|own| own.session_id() == mailbox)
    }

    /// The one rule for what the model may reach: only tasks that report to
    /// the mailbox of its run (`asker`). Outside a run (`None`) it reaches
    /// nothing.
    fn answers(&self, asker: Option<MakiId>) -> bool {
        asker.is_some_and(|asker| self.reports_to(asker))
    }

    /// The notice text the model reads, the line the transcript shows, and
    /// how the task ended.
    fn notice(&self, done: &ToolDoneEvent, output: &str) -> (String, String, TaskEnd) {
        let tool = match &self.info.kind {
            Kind::Call { tool } => tool,
            Kind::Agent => &done.tool,
        };
        result_notice(
            &self.label(tool),
            &self.info.id,
            done.is_error,
            self.stopped.is_some(),
            output,
        )
    }
}

/// What `maki.task.stop(ctx)` hands Lua. `output` is missing when the final
/// reply was late. `name` is how the user knows the task, since the model
/// only knows its id.
#[derive(Debug, PartialEq, Serialize)]
pub(crate) struct ModelStop {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output: Option<String>,
    pub(crate) name: String,
}

/// What `maki.agent.list()` and `/tasks` show.
#[derive(Clone)]
pub(crate) struct BackgroundInfo {
    pub(crate) id: String,
    pub(crate) plugin: Arc<str>,
    /// A moved call's start summary.
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) started_at: u64,
    pub(crate) cost: Option<f64>,
}

/// The one owner of a moved call's end, handed to dispatch as the call's
/// `land`. A final reply delivers, a reply that never comes reports the call
/// lost, and dropping it unlanded (its receipt never reached the model)
/// abandons the call. Each of these and [`BackgroundTasks::lose_all`] takes
/// the entry under one lock, so exactly one settles the receipt.
pub(crate) struct MovedEnd {
    registry: BackgroundTasks,
    id: String,
    ui: Option<UnboundedSender<UiAction>>,
    landed: bool,
}

impl MovedEnd {
    pub(crate) fn new(
        registry: BackgroundTasks,
        id: String,
        ui: Option<UnboundedSender<UiAction>>,
    ) -> Self {
        Self {
            registry,
            id,
            ui,
            landed: false,
        }
    }

    /// A lost call never sends a final result, so nothing else would end its
    /// tab. Only the one that settles the receipt tells it, so it hears once.
    pub(crate) fn land(mut self, done: Option<ToolDoneEvent>) {
        self.landed = true;
        let ui = self.ui.take();
        let Some(done) = done else {
            if let Some(session) = self.registry.lose(&self.id)
                && let Some(ui) = ui
            {
                let _ = ui.send(UiAction::PluginTaskEnded {
                    session,
                    id: self.id.clone(),
                    end: TaskEnd::Lost,
                });
            }
            return;
        };
        self.registry.deliver(&self.id, done, ui);
    }
}

impl Drop for MovedEnd {
    fn drop(&mut self) {
        if !self.landed {
            self.registry.abandon(&self.id);
        }
    }
}

/// Each session handle keeps a clone, and a handle dropped mid-prompt can
/// close from another thread, hence the `Mutex`.
#[derive(Clone, Default)]
pub(crate) struct BackgroundTasks {
    entries: Arc<Mutex<Vec<BackgroundTask>>>,
    /// The model's `task_stop`, parked until the final reply of the call it
    /// stopped. Whoever removes the sender decides who gets the output.
    waiters: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_running_call(entry: &BackgroundTask) -> bool {
    entry.is_call() && entry.stopped.is_none()
}

fn no_running_task_err(id: &str, running: &[String]) -> String {
    let running = if running.is_empty() {
        NONE_RUNNING.to_owned()
    } else {
        format!("{RUNNING_LIST} {}", running.join(", "))
    };
    format!("{NO_RUNNING_TASK} {id}; {running}")
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
    /// outlive its chat session. The same goes for the run a moved call
    /// reports to: [`Self::end_mailbox`] closes its mailbox before it takes
    /// this lock. A moved call's receipt is tracked from here until
    /// [`Self::deliver`] settles it.
    pub(crate) fn register(&self, entry: BackgroundTask) -> Result<(), String> {
        let mut entries = self.locked();
        if AgentSeed::live(entry.session).is_none() {
            return Err(not_live(entry.session));
        }
        if entry.is_call()
            && let Some(mailbox) = &entry.mailbox
            && !mailbox.track(&entry.info.id)
        {
            return Err(RUN_ENDED.to_owned());
        }
        entries.push(entry);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, id: &str) -> bool {
        self.locked().iter().any(|entry| entry.info.id == id)
    }

    pub(crate) fn update(&self, id: &str, apply: impl FnOnce(&mut BackgroundTask)) {
        if let Some(entry) = self.locked().iter_mut().find(|e| e.info.id == id) {
            apply(entry);
        }
    }

    pub(crate) fn stopped_by(&self, id: &str) -> Option<Cutter> {
        self.locked().iter().find(|e| e.info.id == id)?.stopped
    }

    /// Where moved call `id` reports, which the sessions it runs report to as
    /// well, so its caller reaches them too.
    pub(crate) fn mailbox(&self, id: &str) -> Option<SessionMailbox> {
        self.locked()
            .iter()
            .find(|e| e.info.id == id)?
            .mailbox
            .clone()
    }

    /// The live session of subagent `id`, and its name, if the run reading
    /// mailbox `asker` reaches it by [`BackgroundTask::answers`].
    pub(crate) fn subagent(
        &self,
        asker: Option<MakiId>,
        id: &str,
    ) -> Option<(Arc<SessionControl>, String)> {
        let entries = self.locked();
        let entry = entries
            .iter()
            .find(|e| e.info.id == id && e.answers(asker))?;
        let control = entry.control.as_ref()?;
        control
            .ended()
            .is_none()
            .then(|| (Arc::clone(control), entry.info.name.clone()))
    }

    pub(crate) fn remove(&self, id: &str) {
        self.extract(|e| e.info.id == id);
    }

    /// The stop is recorded before the work is cut short, so the work sees
    /// who stopped it. A moved call stays listed until [`Self::deliver`],
    /// which needs its session, plugin and stop. Its work is fired right away,
    /// outside the lock.
    pub(crate) fn stop(&self, id: &str, by: Cutter) {
        let fired = {
            let mut entries = self.locked();
            let Some(entry) = entries.iter_mut().find(|e| e.info.id == id) else {
                return;
            };
            entry.stop(by);
            entry.is_call().then(|| entry.cancel.disarm())
        };
        if fired.is_none() {
            self.remove(id);
        }
    }

    /// The model's stop of one of the calls it moved, those reporting to its
    /// run's `mailbox` (`None` outside a run: it moved none). Waits up to
    /// `wait` for the final reply and returns its text, and then the run gets
    /// no notice of the end. When the reply is late this returns no output,
    /// and the reply still reaches the run as a quiet notice.
    pub(crate) async fn stop_by_model(
        &self,
        mailbox: Option<MakiId>,
        id: &str,
        wait: Duration,
    ) -> Result<ModelStop, String> {
        // Armed before the stop fires, so a call that ignores the stop is
        // abandoned (cancel + the same wait) only after this gives up.
        let deadline = Instant::now() + wait;
        let (mut reply, name) = self.begin_model_stop(mailbox, id)?;
        let arrived = timeout_at(deadline, &mut reply)
            .await
            .ok()
            .and_then(Result::ok);
        let output = arrived.or_else(|| {
            lock(&self.waiters).remove(id);
            reply.try_recv().ok()
        });
        Ok(ModelStop { output, name })
    }

    /// The reply to wait on, and the stopped call's name.
    fn begin_model_stop(
        &self,
        mailbox: Option<MakiId>,
        id: &str,
    ) -> Result<(oneshot::Receiver<String>, String), String> {
        let running: Vec<_> = self
            .locked()
            .iter()
            .filter(|e| is_running_call(e) && e.answers(mailbox))
            .map(|e| e.info.clone())
            .collect();
        let Some(target) = running.iter().find(|info| info.id == id) else {
            let ids: Vec<_> = running.into_iter().map(|info| info.id).collect();
            return Err(no_running_task_err(id, &ids));
        };
        let name = target.name.clone();
        let (waiter, reply) = oneshot::channel();
        lock(&self.waiters).insert(id.to_owned(), waiter);
        self.stop(id, Cutter::Parent);
        Ok((reply, name))
    }

    /// The final reply of a moved call: settles its receipt in the run it
    /// reports to, closes its task tab and retires the entry, which ends the
    /// call but not what it left running past its reply. A model's
    /// `task_stop` that is waiting takes the output instead of the run
    /// getting a notice. Needs no Lua host, so it can land after the host
    /// died.
    pub(crate) fn deliver(
        &self,
        id: &str,
        done: ToolDoneEvent,
        ui: Option<UnboundedSender<UiAction>>,
    ) {
        let Some(task) = self.take(id) else {
            tracing::debug!(id, "final reply of a background task that already settled");
            return;
        };
        task.cancel.finish();
        if let Some(mailbox) = &task.mailbox {
            let output = done.output.as_text();
            if self.hand_to_waiter(id, &output) {
                mailbox.settle(id, None);
            } else {
                let (text, display, end) = task.notice(&done, &output);
                mailbox.settle(
                    id,
                    Some(BackgroundNotice {
                        text,
                        display: Some(&display),
                        wake: task.stopped.is_none(),
                        end,
                        markdown: done.output.is_markdown(),
                    }),
                );
            }
        }
        if let Some(ui) = ui {
            // A killed process usually exits with an error, so the tab hears
            // of the stop first and ends cancelled, not failed.
            if task.stopped.is_some() {
                let _ = ui.send(UiAction::PluginTaskEnded {
                    session: task.session,
                    id: id.to_owned(),
                    end: TaskEnd::Cancelled,
                });
            }
            for event in [
                AgentEvent::ToolDone(Box::new(done)),
                AgentEvent::StreamClosed,
            ] {
                let _ = ui.send(UiAction::PluginTaskEvent {
                    session: task.session,
                    plugin: Arc::clone(&task.info.plugin),
                    envelope: Box::new(Envelope {
                        event,
                        subagent: None,
                        run_id: TASK_EVENT_RUN_ID,
                    }),
                });
            }
        }
    }

    /// Sends the model the result of {part} of moved call `id` now, while the
    /// rest keeps running. It is worded like the final reply of a call that
    /// moved alone, so the transcript reads the same either way. Says `false`
    /// once the call stopped or has its final reply, because that reply will
    /// carry the part and explain the stop. Also `false` when nothing waits
    /// on `id` any more.
    pub(crate) fn report_part(
        &self,
        id: &str,
        part: &TaskLabel,
        failed: bool,
        output: &str,
        markdown: bool,
    ) -> bool {
        let Some(mailbox) = self
            .locked()
            .iter()
            .find(|e| e.info.id == id && is_running_call(e) && !e.has_final())
            .and_then(|e| e.mailbox.clone())
        else {
            return false;
        };
        let (text, display, end) = result_notice(part, id, failed, false, output);
        mailbox.report_part(
            id,
            BackgroundNotice {
                text,
                display: Some(&display),
                wake: true,
                end,
                markdown,
            },
        )
    }

    /// Marks the final reply of moved call `id` as existing, so a host dying
    /// from here on leaves the call to [`Self::deliver`].
    pub(crate) fn final_ready(&self, id: &str) {
        self.update(id, |e| e.replied = true);
    }

    /// Takes the entry out whole, so whoever gets it is the one that settles
    /// it.
    fn take(&self, id: &str) -> Option<BackgroundTask> {
        let mut entries = self.locked();
        let at = entries.iter().position(|e| e.info.id == id)?;
        Some(entries.remove(at))
    }

    /// Under the lock the timeout path takes to withdraw, so the output goes to
    /// exactly one of the stop and the run's mailbox.
    fn hand_to_waiter(&self, id: &str, output: &str) -> bool {
        lock(&self.waiters)
            .remove(id)
            .is_some_and(|waiter| waiter.send(output.to_owned()).is_ok())
    }

    /// A moved call whose receipt never reached the model: the dispatcher
    /// dropped it, and with it the tail that lands the final reply. The work
    /// ends as if it never moved, and its receipt cannot come back as lost on
    /// resume.
    pub(crate) fn abandon(&self, id: &str) {
        self.settle_unanswered(
            |e| e.info.id == id,
            |task, mailbox| {
                mailbox.forget(&task.info.id);
            },
        );
    }

    /// A moved call whose work ended without a final reply, its Lua task
    /// gone with the host or a panic. Returns its chat session, or `None`
    /// when something else settled it first.
    pub(crate) fn lose(&self, id: &str) -> Option<MakiId> {
        self.settle_unanswered(|e| e.info.id == id, BackgroundTask::report_lost)
            .pop()
    }

    pub(crate) fn end_session(&self, session: MakiId) {
        self.retire(|e| e.session == session);
    }

    /// The run reading `mailbox` is gone, so its moved calls have nobody to
    /// report to: they stop, and no call can move there any more.
    pub(crate) fn end_mailbox(&self, mailbox: &SessionMailbox) {
        mailbox.close();
        let id = mailbox.session_id();
        self.retire(|e| e.reports_to(id));
    }

    pub(crate) fn end_task(&self, task: u64) {
        self.retire(|e| e.task == Some(task));
    }

    /// Moved calls outlive their plugin's unload, as in-flight calls do.
    pub(crate) fn end_plugin(&self, plugin: &str) {
        self.retire(|e| e.lifetime == Lifetime::Plugin && &*e.info.plugin == plugin);
    }

    /// The host shut down (`/reload`, exit) and every task died with its
    /// executor, moved calls included. A run holding a receipt for one would
    /// otherwise wait on it for the rest of the process. A call whose final
    /// reply already exists is left to [`Self::deliver`].
    pub(crate) fn lose_all(&self) {
        self.settle_unanswered(|e| !e.has_final(), BackgroundTask::report_lost);
    }

    /// Returns the chat sessions of the entries it settled.
    fn settle_unanswered(
        &self,
        doomed: impl Fn(&BackgroundTask) -> bool,
        settle: impl Fn(&BackgroundTask, &SessionMailbox),
    ) -> Vec<MakiId> {
        self.extract(doomed)
            .into_iter()
            .map(|task| {
                if let Some(mailbox) = &task.mailbox {
                    settle(&task, mailbox);
                }
                task.session
            })
            .collect()
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
    pub(crate) fn list_agents(&self, session: MakiId) -> Vec<(BackgroundInfo, Activity)> {
        self.locked()
            .iter()
            .filter(|e| {
                e.session == session && e.info.kind == Kind::Agent && e.lifetime == Lifetime::Plugin
            })
            .filter_map(|e| Some((e.info.clone(), e.control.as_ref()?.activity())))
            .collect()
    }

    /// A moved call ends as a user stop does: fired now and kept until its
    /// final reply, so [`Self::deliver`] stays the one place that settles its
    /// receipt and closes its task tab. [`Self::lose_all`] covers a host that
    /// dies before the reply.
    fn retire(&self, doomed: impl Fn(&BackgroundTask) -> bool) {
        let (fired, retired): (Vec<_>, Vec<_>) = {
            let mut entries = self.locked();
            let fired = entries
                .iter_mut()
                .filter(|e| doomed(e) && is_running_call(e))
                .map(|e| {
                    e.stop(Cutter::User);
                    e.cancel.disarm()
                })
                .collect();
            let retired = entries
                .extract_if(.., |e| doomed(e) && !e.is_call())
                .collect();
            (fired, retired)
        };
        drop(fired);
        self.drop_waiters(&retired);
    }

    fn extract(&self, doomed: impl Fn(&BackgroundTask) -> bool) -> Vec<BackgroundTask> {
        let extracted: Vec<_> = self.locked().extract_if(.., |e| doomed(e)).collect();
        self.drop_waiters(&extracted);
        extracted
    }

    fn drop_waiters(&self, ended: &[BackgroundTask]) {
        if !ended.is_empty() {
            lock(&self.waiters).retain(|id, _| ended.iter().all(|e| e.info.id != *id));
        }
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
    use maki_providers::TaskSettled;
    use std::iter;
    use test_case::test_case;
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::runtime::test_support::call_scope;
    use crate::runtime::{TaskCell, TaskScope, lock_cell};

    const PLUGIN: &str = "monitor";
    const TOOL: &str = "bash";
    const CALL_ID: &str = "toolu_01";
    const OTHER_CALL_ID: &str = "toolu_02";
    const UNKNOWN_CALL_ID: &str = "missing";
    const SUMMARY: &str = "cargo build";
    const OUTPUT: &str = "compiled";
    const NOTICE_HEAD: &str = "Background";
    const FINISHED: &str = "finished";
    const FAILED: &str = "failed";
    const NOT_A_TASK_EVENT: &str = "expected a plugin task event";
    const FOREVER: Duration = Duration::from_secs(3600);
    const KEPT_UNTIL_DELIVER: &str = "a stopped call stays until its final reply";
    const TAB_CLOSED: &str = "the final reply and the stream end reach the task tab";
    const SETTLED_ONCE: &str = "a receipt settles exactly once";

    enum Retire {
        Session,
        Mailbox,
    }

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
                started_at: 0,
                cost: None,
            },
            session,
            task,
            lifetime: Lifetime::Plugin,
            stopped: None,
            control: Some(Arc::default()),
            replied: false,
            cancel: Fire::Owned(cancel),
            mailbox: None,
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

    /// A call of the chat's own agent, reporting to the chat's mailbox.
    fn call_entry(session: MakiId, id: &str) -> (BackgroundTask, CancelToken) {
        call_entry_to(session, SessionMailbox::open(session).ok(), id)
    }

    fn call_entry_to(
        session: MakiId,
        mailbox: Option<SessionMailbox>,
        id: &str,
    ) -> (BackgroundTask, CancelToken) {
        let (mut entry, token) = entry(session, None);
        entry.info.id = id.to_owned();
        entry.info.name = SUMMARY.to_owned();
        entry.info.kind = Kind::Call {
            tool: Arc::from(TOOL),
        };
        entry.control = None;
        entry.lifetime = Lifetime::Call;
        entry.mailbox = mailbox;
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

    fn registry_with_call(session: MakiId) -> (BackgroundTasks, CancelToken) {
        let registry = BackgroundTasks::default();
        let (call, token) = call_entry(session, CALL_ID);
        registry.register(call).unwrap();
        (registry, token)
    }

    #[test]
    fn plugin_unload_spares_moved_calls() {
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
    }

    #[test]
    fn stopping_a_session_entry_removes_it_at_once() {
        let session = MakiId::generate();
        let _live = live(session);
        let registry = BackgroundTasks::default();
        let (opened, token) = entry(session, None);
        let id = opened.info.id.clone();
        registry.register(opened).unwrap();

        registry.stop(&id, Cutter::User);

        assert!(token.is_cancelled());
        assert!(!registry.contains(&id));
    }

    /// The owner token carries what the call left running, like a highlight.
    #[test_case(false ; "a_delivered_call_leaves_it_running")]
    #[test_case(true ; "a_stopped_call_takes_it_along")]
    fn a_moved_calls_late_work(stopped: bool) {
        let session = MakiId::generate();
        let _live = live(session);
        let (call, _run, trigger) = call_scope(true);
        let (mut entry, _) = call_entry(session, CALL_ID);
        entry.cancel = Fire::Call(trigger.expect("moved"));
        let registry = BackgroundTasks::default();
        registry.register(entry).unwrap();

        if stopped {
            registry.stop(CALL_ID, Cutter::User);
        }
        registry.deliver(CALL_ID, done(OUTPUT, false), None);

        assert!(call.cancel().is_cancelled());
        assert_eq!(call.owner().is_cancelled(), stopped);
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
            .map(|(info, _)| info.id)
            .collect();

        assert_eq!(ids, [opened_id]);
        assert_eq!(registry.list(session).len(), 2);
    }

    #[test_case(None, false, FINISHED, TaskEnd::Finished, true ; "finished_wakes_the_chat")]
    #[test_case(None, true, FAILED, TaskEnd::Failed, true ; "failed_wakes_the_chat")]
    #[test_case(Some(Cutter::User), true, FAILED, TaskEnd::Cancelled, false ; "stopped_by_the_user_is_quiet")]
    fn deliver_tells_the_chat(
        stop: Option<Cutter>,
        is_error: bool,
        text_outcome: &str,
        end: TaskEnd,
        wakes: bool,
    ) {
        let session = MakiId::generate();
        let (mailbox, _seed) = live(session);
        let (registry, token) = registry_with_call(session);
        if let Some(stop) = stop {
            registry.stop(CALL_ID, stop);
            assert!(token.is_cancelled());
            assert!(registry.contains(CALL_ID), "{KEPT_UNTIL_DELIVER}");
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
                format!("{NOTICE_HEAD} {TOOL} {CALL_ID} ({SUMMARY}) {text_outcome}:\n\n{OUTPUT}")
                    .as_str()
            )
        );
        assert_eq!(
            notices[0].settles(),
            Some(&TaskSettled {
                task: CALL_ID.into(),
                end
            })
        );
        assert!(!registry.contains(CALL_ID));
    }

    #[test_case(ToolOutput::Markdown(OUTPUT.into()), true ; "markdown_reply")]
    #[test_case(ToolOutput::Plain(OUTPUT.into()), false ; "plain_output")]
    fn a_delivered_notice_keeps_the_output_format(output: ToolOutput, markdown: bool) {
        let session = MakiId::generate();
        let (mailbox, _seed) = live(session);
        let (registry, _token) = registry_with_call(session);

        let done = ToolDoneEvent {
            output: Arc::new(output),
            ..done(OUTPUT, false)
        };
        registry.deliver(CALL_ID, done, None);

        assert_eq!(mailbox.claim_wake()[0].markdown, markdown);
    }

    #[test]
    fn deliver_closes_the_task_tab_with_the_final_reply() {
        let session = MakiId::generate();
        let _live = live(session);
        let (registry, ..) = registry_with_call(session);
        let (tx, mut rx) = unbounded_channel();

        registry.deliver(CALL_ID, done(OUTPUT, false), Some(tx));

        let events: Vec<_> = iter::from_fn(|| rx.try_recv().ok())
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

    #[test_case(Cutter::User ; "by_the_user")]
    #[test_case(Cutter::Parent ; "by_the_model")]
    fn a_stopped_calls_tab_hears_of_the_stop_before_its_reply(stop: Cutter) {
        let session = MakiId::generate();
        let _live = live(session);
        let (registry, ..) = registry_with_call(session);
        let (tx, mut rx) = unbounded_channel();
        registry.stop(CALL_ID, stop);

        registry.deliver(CALL_ID, done(OUTPUT, true), Some(tx));

        let first = rx.try_recv().expect("the tab hears of the stop");
        assert!(matches!(
            first,
            UiAction::PluginTaskEnded { session: chat, id, end: TaskEnd::Cancelled } if chat == session && id == CALL_ID
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(UiAction::PluginTaskEvent { envelope, .. }) if matches!(envelope.event, AgentEvent::ToolDone(_))
        ));
    }

    #[tokio::test]
    async fn model_stop_that_gets_the_output_leaves_the_chat_no_notice() {
        let session = MakiId::generate();
        let (mailbox, _seed) = live(session);
        let (registry, token) = registry_with_call(session);

        let (stopped, ()) = tokio::join!(
            registry.stop_by_model(Some(session), CALL_ID, FOREVER),
            async {
                assert_eq!(registry.stopped_by(CALL_ID), Some(Cutter::Parent));
                registry.deliver(CALL_ID, done(OUTPUT, true), None)
            },
        );

        assert_eq!(stopped.map(|stop| stop.output), Ok(Some(OUTPUT.to_owned())));
        assert!(token.is_cancelled());
        assert!(mailbox.unsettled().is_empty());
        assert!(mailbox.drain().is_empty());
        assert!(!registry.contains(CALL_ID));
    }

    #[tokio::test]
    async fn model_stop_that_times_out_gets_the_late_output_as_a_quiet_notice() {
        let session = MakiId::generate();
        let (mailbox, _seed) = live(session);
        let (registry, ..) = registry_with_call(session);

        let stopped = registry
            .stop_by_model(Some(session), CALL_ID, Duration::ZERO)
            .await;

        assert_eq!(stopped.map(|stop| stop.output), Ok(None));
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
        drop(registry.begin_model_stop(Some(session), CALL_ID).unwrap());

        registry.deliver(CALL_ID, done(OUTPUT, false), None);

        assert_eq!(mailbox.drain().len(), 1);
    }

    enum Asker {
        Owner,
        /// A subagent of the same chat, with its own run.
        Subagent,
        /// Outside any run.
        NoRun,
    }

    #[test_case(Asker::Owner, UNKNOWN_CALL_ID, true ; "unknown_id_lists_the_runs_tasks")]
    #[test_case(Asker::Subagent, CALL_ID, false ; "another_runs_id_is_refused")]
    #[test_case(Asker::NoRun, CALL_ID, false ; "no_run_moved_nothing")]
    #[tokio::test]
    async fn model_stop_refuses_what_the_callers_run_did_not_move(
        asker: Asker,
        target: &str,
        lists_running: bool,
    ) {
        let chat = MakiId::generate();
        let _live = live(chat);
        let subagent = SessionMailbox::register(MakiId::generate());
        let registry = BackgroundTasks::default();
        let tokens = [CALL_ID, OTHER_CALL_ID].map(|id| {
            let (call, token) = call_entry(chat, id);
            registry.register(call).unwrap();
            token
        });
        let asker = match asker {
            Asker::Owner => Some(chat),
            Asker::Subagent => Some(subagent.session_id()),
            Asker::NoRun => None,
        };

        let stopped = registry.stop_by_model(asker, target, FOREVER).await;

        let listing = if lists_running {
            format!("{RUNNING_LIST} {CALL_ID}, {OTHER_CALL_ID}")
        } else {
            NONE_RUNNING.to_owned()
        };
        assert_eq!(
            stopped,
            Err(format!("{NO_RUNNING_TASK} {target}; {listing}"))
        );
        assert!(tokens.iter().all(|token| !token.is_cancelled()));
    }

    enum Target {
        Live,
        Ended,
        NoSession,
    }

    /// `task_message` and `maki.task.messages` reach a subagent by the rule
    /// `task_stop` follows, and only while its session lives.
    #[test_case(Asker::Owner, Target::Live, true ; "the_owner_reaches_a_live_session")]
    #[test_case(Asker::Subagent, Target::Live, false ; "another_run_does_not")]
    #[test_case(Asker::NoRun, Target::Live, false ; "outside_a_run_nothing_does")]
    #[test_case(Asker::Owner, Target::Ended, false ; "an_ended_session_is_gone")]
    #[test_case(Asker::Owner, Target::NoSession, false ; "a_call_without_a_session")]
    fn subagent_follows_the_model_stop_rule(asker: Asker, target: Target, reached: bool) {
        let chat = MakiId::generate();
        let _live = live(chat);
        let subagent = SessionMailbox::register(MakiId::generate());
        let registry = BackgroundTasks::default();
        let (mut call, _token) = call_entry(chat, CALL_ID);
        let control = Arc::new(SessionControl::default());
        if !matches!(target, Target::NoSession) {
            call.attach(Arc::clone(&control));
        }
        if matches!(target, Target::Ended) {
            control.end(EndReason::Closed);
        }
        registry.register(call).unwrap();
        let asker = match asker {
            Asker::Owner => Some(chat),
            Asker::Subagent => Some(subagent.session_id()),
            Asker::NoRun => None,
        };

        let found = registry.subagent(asker, CALL_ID);

        assert_eq!(found.is_some(), reached);
        if let Some((found, name)) = found {
            assert!(Arc::ptr_eq(&found, &control));
            assert_eq!(name, SUMMARY);
        }
    }

    /// The session a moved call runs hears of the call's stop whichever came
    /// first, and the stop still counts when that session was already over.
    #[test_case(true, false ; "session_attached_before_the_stop")]
    #[test_case(false, false ; "session_attached_after_the_stop")]
    #[test_case(true, true ; "session_closed_before_the_stop")]
    fn a_moved_calls_stop_reaches_its_session_in_either_order(attached_first: bool, closed: bool) {
        let session = MakiId::generate();
        let _live = live(session);
        let (registry, _token) = registry_with_call(session);
        let control = Arc::new(SessionControl::default());
        let attach = || registry.update(CALL_ID, |e| e.attach(Arc::clone(&control)));
        if attached_first {
            attach();
        }
        if closed {
            control.end(EndReason::Closed);
        }

        registry.stop(CALL_ID, Cutter::Parent);
        if !attached_first {
            attach();
        }

        assert_eq!(registry.stopped_by(CALL_ID), Some(Cutter::Parent));
        let ended = if closed {
            EndReason::Closed
        } else {
            EndReason::Stopped(Cutter::Parent)
        };
        assert_eq!(control.ended(), Some(ended));
    }

    #[test]
    fn end_mailbox_stops_the_calls_reporting_there_and_leaves_the_rest() {
        let chat = MakiId::generate();
        let (chat_mailbox, _seed) = live(chat);
        let subagent = SessionMailbox::register(MakiId::generate());
        let registry = BackgroundTasks::default();
        let (ended, ended_token) = call_entry_to(chat, Some(subagent.clone()), CALL_ID);
        let (kept, kept_token) = call_entry(chat, OTHER_CALL_ID);
        registry.register(ended).unwrap();
        registry.register(kept).unwrap();

        registry.end_mailbox(&subagent);

        assert!(ended_token.is_cancelled());
        assert!(registry.contains(CALL_ID), "{KEPT_UNTIL_DELIVER}");
        assert!(subagent.drain().is_empty());
        assert!(!kept_token.is_cancelled());
        assert!(registry.contains(OTHER_CALL_ID));
        assert_eq!(chat_mailbox.unsettled(), [OTHER_CALL_ID]);
    }

    /// Retiring ends a moved call the way a user stop does, so its final
    /// reply still settles the receipt quietly and closes its task tab.
    #[test_case(Retire::Session ; "session_end")]
    #[test_case(Retire::Mailbox ; "run_end")]
    fn a_retired_moved_call_reports_its_end_quietly(retire: Retire) {
        let session = MakiId::generate();
        let (mailbox, _seed) = live(session);
        let (registry, token) = registry_with_call(session);
        let (tx, mut rx) = unbounded_channel();

        match retire {
            Retire::Session => registry.end_session(session),
            Retire::Mailbox => registry.end_mailbox(&mailbox),
        }

        assert!(token.is_cancelled());
        assert_eq!(registry.stopped_by(CALL_ID), Some(Cutter::User));
        assert!(registry.contains(CALL_ID), "{KEPT_UNTIL_DELIVER}");

        registry.deliver(CALL_ID, done(OUTPUT, true), Some(tx));

        assert!(!registry.contains(CALL_ID));
        assert!(mailbox.claim_wake().is_empty());
        assert_eq!(mailbox.drain().len(), 1);
        let tab_events = iter::from_fn(|| rx.try_recv().ok())
            .filter(|action| matches!(action, UiAction::PluginTaskEvent { .. }))
            .count();
        assert_eq!(tab_events, 2, "{TAB_CLOSED}");
    }

    /// `/reload` ends every session and then the host: work stopped by the
    /// first never delivers, so the second reports it lost.
    #[test]
    fn a_host_dying_after_session_end_reports_the_call_lost() {
        let session = MakiId::generate();
        let (mailbox, _seed) = live(session);
        let (registry, ..) = registry_with_call(session);

        registry.end_session(session);
        registry.lose_all();

        assert_eq!(mailbox.outstanding(), 0);
        assert_eq!(mailbox.unsettled(), [CALL_ID]);
        assert_eq!(mailbox.drain().len(), 1);
    }

    /// Whether the one notice a call settled with carries its output, rather
    /// than reporting it lost.
    fn delivered_output(mailbox: &SessionMailbox) -> bool {
        let notices = mailbox.drain();
        assert_eq!(notices.len(), 1, "{SETTLED_ONCE}");
        let text = notices[0].first_text_content().expect("a notice has text");
        assert!(text.contains(CALL_ID), "{text}");
        text.ends_with(OUTPUT)
    }

    /// `/reload!` while a finished call's output layer still runs: the reply
    /// exists and delivering it needs no host, so the host dying leaves it be.
    #[test]
    fn host_shutdown_leaves_a_call_whose_final_reply_exists_to_deliver() {
        let session = MakiId::generate();
        let (mailbox, _seed) = live(session);
        let (registry, ..) = registry_with_call(session);

        registry.final_ready(CALL_ID);
        registry.lose_all();

        assert!(registry.contains(CALL_ID));
        assert_eq!(mailbox.outstanding(), 1);

        registry.deliver(CALL_ID, done(OUTPUT, false), None);

        assert!(!registry.contains(CALL_ID));
        assert!(delivered_output(&mailbox));
    }

    enum Ending {
        Delivered,
        Lost,
        Abandoned,
    }

    /// Whichever of the call's own end and the host's shutdown comes first
    /// settles the receipt, and the other finds nothing left to settle.
    #[test_case(Ending::Delivered, true,  Some(true)  ; "delivered_before_the_host_died")]
    #[test_case(Ending::Delivered, false, Some(false) ; "lost_with_the_host_before_its_reply")]
    #[test_case(Ending::Lost,      true,  Some(false) ; "lost_before_the_host_died")]
    #[test_case(Ending::Lost,      false, Some(false) ; "lost_with_the_host")]
    #[test_case(Ending::Abandoned, true,  None        ; "abandoned_before_the_host_died")]
    fn a_moved_end_settles_the_receipt_once(
        ending: Ending,
        ends_first: bool,
        delivered: Option<bool>,
    ) {
        let session = MakiId::generate();
        let (mailbox, _seed) = live(session);
        let (registry, token) = registry_with_call(session);
        let end = MovedEnd::new(registry.clone(), CALL_ID.to_owned(), None);
        let finish = move || match ending {
            Ending::Delivered => end.land(Some(done(OUTPUT, false))),
            Ending::Lost => end.land(None),
            Ending::Abandoned => drop(end),
        };

        if ends_first {
            finish();
            registry.lose_all();
        } else {
            registry.lose_all();
            finish();
        }

        assert!(token.is_cancelled());
        assert!(!registry.contains(CALL_ID));
        assert_eq!(mailbox.outstanding(), 0);
        match delivered {
            Some(delivered) => assert_eq!(delivered_output(&mailbox), delivered),
            None => {
                assert!(mailbox.drain().is_empty());
                assert!(mailbox.unsettled().is_empty());
            }
        }
    }

    /// When the host died first, it already settled the receipt, so the
    /// late landing must stay quiet or the tab hears twice.
    #[test_case(true  ; "lost_on_its_own")]
    #[test_case(false ; "after_the_host_died")]
    fn a_lost_calls_tab_hears_of_it_from_whoever_settled_it(settles_it: bool) {
        let session = MakiId::generate();
        let _live = live(session);
        let (registry, ..) = registry_with_call(session);
        let (tx, mut rx) = unbounded_channel();
        if !settles_it {
            registry.lose_all();
        }

        MovedEnd::new(registry, CALL_ID.to_owned(), Some(tx)).land(None);

        let told = iter::from_fn(|| rx.try_recv().ok()).any(|action| {
            matches!(
                action,
                UiAction::PluginTaskEnded { session: chat, id, end: TaskEnd::Lost }
                    if chat == session && id == CALL_ID
            )
        });
        assert_eq!(told, settles_it);
    }
}
