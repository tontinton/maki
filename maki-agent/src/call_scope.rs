//! One model tool call's cancel subtree and the answer to "was it moved to the
//! background?". Nested calls share the scope, so everything a call started is
//! one subtree with one `home` to read.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use event_listener::{Event, EventListener};
use maki_storage::id::MakiId;
use thiserror::Error;

use crate::cancel::{CancelToken, CancelTrigger};
use crate::session_control::SessionControl;
use crate::types::SubagentInfo;

/// Where a moved call lives once its turn no longer owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskHome {
    pub chat: MakiId,
    pub id: String,
    pub plugin: Arc<str>,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("call already moved to the background")]
pub struct AlreadyMoved;

/// A subagent session a call opened through its ctx. A move gives each one
/// its own background entry, so every adopted tab can be stopped by its id.
#[derive(Clone)]
pub struct SessionStub {
    pub ui_id: String,
    pub plugin: Arc<str>,
    pub name: String,
    pub control: Arc<SessionControl>,
    /// Set at the session's first prompt, when the UI first hears of it.
    pub info: Arc<OnceLock<SubagentInfo>>,
}

pub struct CallScope {
    id: String,
    run: CancelToken,
    /// What the call leaves running past its reply, like a highlight, lives
    /// here, so the call returning never fires it. The run ending does. After
    /// a move there is no run to follow, so it follows the call token: a stop
    /// still takes it along, but a final reply lets it go
    /// ([`MovedTrigger::finish`]).
    owner: CancelToken,
    cancel: CancelToken,
    trigger: Mutex<Option<CancelTrigger>>,
    home: OnceLock<TaskHome>,
    moved: Event,
    sessions: Mutex<Vec<SessionStub>>,
}

impl CallScope {
    /// {run} is the run token the call token descends from until a move.
    pub fn new(id: String, run: &CancelToken) -> Self {
        let (trigger, cancel) = run.child();
        Self {
            id,
            run: run.clone(),
            owner: run.follower(),
            cancel,
            trigger: Mutex::new(Some(trigger)),
            home: OnceLock::new(),
            moved: Event::new(),
            sessions: Mutex::new(Vec::new()),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn cancel(&self) -> &CancelToken {
        &self.cancel
    }

    pub fn owner(&self) -> &CancelToken {
        &self.owner
    }

    pub fn home(&self) -> Option<&TaskHome> {
        self.home.get()
    }

    /// Notified once, by the move. Check [`home`](Self::home) after creating
    /// the listener to close the race with a move in between.
    pub fn moved_listener(&self) -> EventListener {
        self.moved.listen()
    }

    pub async fn moved(&self) {
        let listener = self.moved_listener();
        if self.home().is_none() {
            listener.await;
        }
    }

    /// Takes the gate of a move, detaches the subtree from the run token and
    /// hands the call's [`MovedTrigger`] to the caller, who now decides when
    /// the work ends. Detached here rather than at the landing, so the run
    /// ending as the receipt reaches it finds nothing left to cancel.
    ///
    /// Taking the trigger is the one gate: a second move, or a move after the
    /// call ended and its guard fired, finds it gone. A run token that already
    /// fired has cancelled the call token by the time this returns.
    pub fn claim_move(&self) -> Result<(MoveClaim<'_>, MovedTrigger), AlreadyMoved> {
        let call = self.take_trigger().ok_or(AlreadyMoved)?;
        // In this order a run firing midway reaches the owner either way:
        // directly, or through the call token it now follows.
        self.cancel.adopt(&self.owner);
        self.owner.unlink(&self.run);
        self.cancel.unlink(&self.run);
        let owner = self.owner.clone();
        Ok((MoveClaim(self), MovedTrigger { call, owner }))
    }

    /// A move with nothing to do between the claim and the landing.
    pub fn move_to(&self, home: TaskHome) -> Result<MovedTrigger, AlreadyMoved> {
        let (claim, trigger) = self.claim_move()?;
        claim.land(home);
        Ok(trigger)
    }

    /// Records a session opened under this call. Once the call has moved the
    /// stub comes back: the mover already took the others, so the caller
    /// registers this one itself. `home` is set before the mover takes the
    /// stubs, and both sides look under the same lock, so none is missed.
    pub fn add_session(&self, stub: SessionStub) -> Option<SessionStub> {
        let mut sessions = self.sessions();
        if self.home().is_some() {
            return Some(stub);
        }
        sessions.push(stub);
        None
    }

    /// A session that closed has nothing left to stop.
    pub fn remove_session(&self, ui_id: &str) {
        self.sessions().retain(|stub| stub.ui_id != ui_id);
    }

    /// The mover's half of [`add_session`](Self::add_session), called after
    /// [`move_to`](Self::move_to).
    pub fn take_sessions(&self) -> Vec<SessionStub> {
        std::mem::take(&mut *self.sessions())
    }

    fn sessions(&self) -> MutexGuard<'_, Vec<SessionStub>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ends the call unless a move took it first, which is then the one that
    /// answers the dispatcher. `false` when the move won.
    pub fn end(&self) -> bool {
        self.take_trigger().map(CancelTrigger::cancel).is_some()
    }

    fn take_trigger(&self) -> Option<CancelTrigger> {
        self.trigger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

/// A move holding the gate that has not landed: the call reads as unmoved.
/// Its holder does every step that can fail first and lands last, so a move
/// that gives up never shows as moved. Giving up drops the trigger too, which
/// ends the call as cancelled.
#[must_use]
pub struct MoveClaim<'a>(&'a CallScope);

impl MoveClaim<'_> {
    pub fn land(self, home: TaskHome) {
        // Only the holder of the trigger sets the home, so this cannot fail.
        let _ = self.0.home.set(home);
        self.0.moved.notify(usize::MAX);
    }
}

/// The end of a moved call, held by whoever runs it now. Dropping it is a
/// stop: it ends the call and everything the call left running.
pub struct MovedTrigger {
    call: CancelTrigger,
    owner: CancelToken,
}

impl MovedTrigger {
    /// The work ended with its final reply, not a stop. A batch whose last
    /// child settles queues that child's highlight right as it returns, so
    /// the highlight must outlive the call, just like for a call that never
    /// moved.
    pub fn finish(&self) {
        self.owner.unlink(&self.call.token());
    }
}

/// Fires the call's trigger when the call ends, unless a move took it first.
pub struct CallGuard(Arc<CallScope>);

impl CallGuard {
    pub fn new(scope: Arc<CallScope>) -> Self {
        Self(scope)
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        self.0.end();
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use futures::FutureExt;
    use test_case::test_case;

    use super::*;
    use crate::tools::BackgroundFor;

    const CALL_ID: &str = "toolu_1";
    const PLUGIN: &str = "bash";

    fn home(chat: MakiId) -> TaskHome {
        TaskHome {
            chat,
            id: CALL_ID.to_owned(),
            plugin: Arc::from(PLUGIN),
        }
    }

    fn scope_under(run: &CancelToken) -> (Arc<CallScope>, CallGuard) {
        let scope = Arc::new(CallScope::new(CALL_ID.to_owned(), run));
        let guard = CallGuard::new(Arc::clone(&scope));
        (scope, guard)
    }

    #[test_case(false ; "a_stop_takes_the_owner_along")]
    #[test_case(true ; "a_final_reply_leaves_the_owner_running")]
    fn a_moved_call_ends_with_its_trigger_alone(finished: bool) {
        let (run_trigger, run) = CancelToken::new();
        let (scope, guard) = scope_under(&run);
        let (_nested_trigger, nested) = scope.cancel().child();
        let trigger = scope.move_to(home(MakiId::generate())).unwrap();

        drop(guard);
        run_trigger.cancel();
        for token in [scope.cancel(), &nested, scope.owner()] {
            assert!(
                !token.is_cancelled(),
                "the call's end or the run's reached it"
            );
        }

        if finished {
            trigger.finish();
        }
        drop(trigger);
        assert!(scope.cancel().is_cancelled());
        assert!(nested.is_cancelled());
        assert_eq!(scope.owner().is_cancelled(), !finished);
    }

    #[test]
    fn a_claim_that_never_lands_leaves_the_call_unmoved_and_its_end_to_the_trigger() {
        let (run_trigger, run) = CancelToken::new();
        let (scope, guard) = scope_under(&run);
        let (claim, trigger) = scope.claim_move().unwrap();
        drop(claim);

        assert!(scope.home().is_none());
        assert!(matches!(scope.claim_move(), Err(AlreadyMoved)));
        drop(guard);
        run_trigger.cancel();
        assert!(!scope.cancel().is_cancelled());
        drop(trigger);
        assert!(scope.cancel().is_cancelled());
    }

    #[test]
    fn the_owner_outlives_the_call_until_the_run_ends() {
        let (run_trigger, run) = CancelToken::new();
        let (scope, guard) = scope_under(&run);

        drop(guard);
        assert!(!scope.owner().is_cancelled());
        run_trigger.cancel();
        assert!(scope.owner().is_cancelled());
    }

    enum Before {
        Moved,
        Ended,
    }

    #[test_case(Before::Moved ; "a_second_move")]
    #[test_case(Before::Ended ; "a_move_after_the_call_ended")]
    fn a_move_without_the_trigger_is_refused(before: Before) {
        let (_run_trigger, run) = CancelToken::new();
        let (scope, guard) = scope_under(&run);
        let _first = match before {
            Before::Moved => Some(scope.move_to(home(MakiId::generate())).unwrap()),
            Before::Ended => {
                drop(guard);
                None
            }
        };
        assert!(matches!(
            scope.move_to(home(MakiId::generate())),
            Err(AlreadyMoved)
        ));
    }

    #[tokio::test]
    async fn a_move_wakes_its_waiters_and_names_the_chat() {
        let (_run_trigger, run) = CancelToken::new();
        let (scope, _guard) = scope_under(&run);
        let background_for = BackgroundFor::Call(Arc::clone(&scope));
        let mut moved = pin!(scope.moved());
        assert!(moved.as_mut().now_or_never().is_none());
        assert_eq!(background_for.chat(), None);

        let chat = MakiId::generate();
        let _trigger = scope.move_to(home(chat)).unwrap();
        assert!(moved.as_mut().now_or_never().is_some());
        assert_eq!(background_for.chat(), Some(chat));
    }
}
