//! Cooperative cancellation with parent-to-child propagation.
//!
//! `CancelTrigger` fires on Drop, so cleanup happens even if the trigger is forgotten.
//! `cancelled()` uses a double-check around the listener to close the TOCTOU window between flag read and listener registration.
//! A parent holds its children weakly and fires them itself, so propagation needs no background task and a dropped child leaves nothing behind.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use event_listener::Event;

#[derive(Default)]
struct Shared {
    cancelled: AtomicBool,
    event: Event,
    children: Mutex<Vec<Weak<Shared>>>,
}

impl Shared {
    fn fire(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.event.notify(usize::MAX);
        let children = std::mem::take(&mut *self.children());
        for child in children.iter().filter_map(Weak::upgrade) {
            child.fire();
        }
    }

    fn children(&self) -> MutexGuard<'_, Vec<Weak<Shared>>> {
        self.children.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Clone)]
pub struct CancelToken(Arc<Shared>);

pub struct CancelTrigger(Arc<Shared>);

/// Fires a token it does not own: dropping it fires nothing.
#[derive(Clone)]
pub struct CancelHandle(Weak<Shared>);

impl CancelToken {
    pub fn new() -> (CancelTrigger, Self) {
        let shared = Arc::<Shared>::default();
        (CancelTrigger(Arc::clone(&shared)), Self(shared))
    }

    pub fn none() -> Self {
        Self(Arc::default())
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub async fn race<T>(&self, future: impl Future<Output = T>) -> Result<T, String> {
        if self.is_cancelled() {
            return Err("cancelled".into());
        }
        tokio::select! {
            output = future => Ok(output),
            () = self.cancelled() => Err("cancelled".into()),
        }
    }

    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let listener = self.0.event.listen();
            if self.is_cancelled() {
                return;
            }
            listener.await;
        }
    }

    pub fn child(&self) -> (CancelTrigger, Self) {
        let (child_trigger, child_token) = Self::new();
        self.adopt(&child_token);
        (child_trigger, child_token)
    }

    /// A child with no trigger of its own: only a parent fires it.
    pub fn follower(&self) -> Self {
        let token = Self::none();
        self.adopt(&token);
        token
    }

    /// Makes {child} fire with this token, as well as with whatever already
    /// fires it.
    ///
    /// The flag is checked under the children lock, and `fire` raises the
    /// flag before it takes that lock. So a child linked while the parent is
    /// firing is either in the list `fire` drains or sees the flag here.
    pub fn adopt(&self, child: &CancelToken) {
        let mut children = self.0.children();
        if self.is_cancelled() {
            child.0.fire();
        } else {
            children.retain(|child| child.strong_count() > 0);
            children.push(Arc::downgrade(&child.0));
        }
    }

    /// Removes this token from {parent}'s children, so {parent} firing no
    /// longer reaches it. Mirrors [`adopt`](Self::adopt): under a parent that
    /// already fired, this token is fired here, so it never escapes a cancel
    /// that `fire` has not drained yet.
    pub fn unlink(&self, parent: &CancelToken) {
        let mut children = parent.0.children();
        if parent.is_cancelled() {
            self.0.fire();
            return;
        }
        let own = Arc::as_ptr(&self.0);
        children.retain(|child| child.strong_count() > 0 && child.as_ptr() != own);
    }

    #[cfg(test)]
    pub(crate) fn same_as(&self, other: &CancelToken) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl CancelTrigger {
    pub fn cancel(self) {
        self.0.fire();
    }

    /// Whether dropping this trigger is what fires {token}. A registry keeps
    /// the triggers and hands out the tokens, so this is how an owner finds its
    /// own row again without an id that somebody else could reuse.
    pub fn fires(&self, token: &CancelToken) -> bool {
        Arc::ptr_eq(&self.0, &token.0)
    }

    /// True once this trigger or a parent of its token fired.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub fn handle(&self) -> CancelHandle {
        CancelHandle(Arc::downgrade(&self.0))
    }
}

impl CancelHandle {
    pub fn fire(&self) {
        if let Some(shared) = self.0.upgrade() {
            shared.fire();
        }
    }
}

impl Drop for CancelTrigger {
    fn drop(&mut self) {
        self.0.fire();
    }
}

/// Names one registration inside a key's list so its owner can retire it
/// without disturbing the others registered under the same key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CancelSlot(u64);

enum Registration {
    /// Dropping it fires the token of the session that registered it.
    Owned(CancelTrigger),
    /// The session owns its trigger, so only an explicit cancel fires it.
    Handle(CancelHandle),
}

impl Registration {
    fn fire(self) {
        match self {
            Self::Owned(trigger) => trigger.cancel(),
            Self::Handle(handle) => handle.fire(),
        }
    }
}

#[derive(Default)]
struct Entry {
    registrations: Vec<(CancelSlot, Registration)>,
    cancelled: bool,
}

/// Triggers grouped under an id a user can name and stop from the outside. One
/// subagent tool call can open several sessions under a single `tool_use_id`,
/// and cancelling that id has to reach all of them, even the ones opened after
/// the cancel.
///
/// So the mark lives on the id until [`cancel_all`](Self::cancel_all) drains
/// the whole map or [`revive`](Self::revive) lifts it. Clearing it when the
/// last sibling retires would lose the cancels that land while the id sits
/// empty, which it does before the first session registers and again between
/// two sessions one tool call opens back to back.
pub struct CancelMap<K> {
    entries: Mutex<HashMap<K, Entry>>,
    next_slot: AtomicU64,
}

impl<K: Eq + std::hash::Hash> Default for CancelMap<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + std::hash::Hash> CancelMap<K> {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            next_slot: AtomicU64::new(0),
        }
    }

    /// Registers {trigger} under {id}, alongside any already there, and
    /// returns the slot to hand back to [`retire`](Self::retire).
    pub fn insert(&self, id: K, trigger: CancelTrigger) -> CancelSlot {
        self.register(id, Registration::Owned(trigger))
    }

    /// Like [`insert`](Self::insert), but the map never owns the token's
    /// lifetime: retiring or [`cancel_all`](Self::cancel_all) leave it running.
    pub fn insert_handle(&self, id: K, handle: CancelHandle) -> CancelSlot {
        self.register(id, Registration::Handle(handle))
    }

    fn register(&self, id: K, registration: Registration) -> CancelSlot {
        let mut map = self.lock();
        let slot = CancelSlot(self.next_slot.fetch_add(1, Ordering::Relaxed));
        let entry = map.entry(id).or_default();
        if entry.cancelled {
            registration.fire();
        } else {
            entry.registrations.push((slot, registration));
        }
        slot
    }

    /// Retires one registration: an owned trigger is dropped, which fires it,
    /// a handle is forgotten. Siblings and the id's cancelled mark stay.
    pub fn retire(&self, id: &K, slot: CancelSlot) {
        let mut map = self.lock();
        let Some(entry) = map.get_mut(id) else {
            return;
        };
        entry
            .registrations
            .retain(|&(registered, _)| registered != slot);
    }

    /// Cancels every registration under {id} and marks the id, so a session
    /// registering under it later is born cancelled too.
    pub fn cancel(&self, id: K) {
        let mut map = self.lock();
        let entry = map.entry(id).or_default();
        entry.cancelled = true;
        for (_, registration) in entry.registrations.drain(..) {
            registration.fire();
        }
    }

    /// A plugin that loads again needs this, or every task it starts would be
    /// born cancelled.
    pub fn revive(&self, id: &K) {
        if let Some(entry) = self.lock().get_mut(id) {
            entry.cancelled = false;
        }
    }

    /// The run that owned these is over: stop the owned triggers, forget the
    /// handles without firing them, and drop the marks, so no cancel of this
    /// run leaks into the next one. A handle's session that is still bound to
    /// the run dies through the token chain anyway, and a moved one must live.
    pub fn cancel_all(&self) {
        self.lock().clear();
    }

    #[cfg(test)]
    fn has_key(&self, id: &K) -> bool {
        self.lock().contains_key(id)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<K, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[tokio::test]
    async fn trigger_wakes_token() {
        let (trigger, token) = CancelToken::new();
        assert!(!token.is_cancelled());
        trigger.cancel();
        token.cancelled().await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn child_cancelled_by_parent() {
        let (parent_trigger, parent_token) = CancelToken::new();
        let (_child_trigger, child_token) = parent_token.child();
        parent_trigger.cancel();
        child_token.cancelled().await;
        assert!(child_token.is_cancelled());
    }

    #[tokio::test]
    async fn child_cancelled_by_own_trigger() {
        let (_parent_trigger, parent_token) = CancelToken::new();
        let (child_trigger, child_token) = parent_token.child();
        child_trigger.cancel();
        child_token.cancelled().await;
        assert!(child_token.is_cancelled());
        assert!(!parent_token.is_cancelled());
    }

    #[test]
    fn child_of_cancelled_parent_is_born_cancelled() {
        let (trigger, parent) = CancelToken::new();
        trigger.cancel();
        let (_child_trigger, child) = parent.child();
        assert!(child.is_cancelled());
    }

    #[test]
    fn a_follower_moved_to_another_parent_fires_only_with_it() {
        let (first_trigger, first) = CancelToken::new();
        let (second_trigger, second) = CancelToken::new();
        let follower = first.follower();
        second.adopt(&follower);
        follower.unlink(&first);

        first_trigger.cancel();
        assert!(!follower.is_cancelled());
        second_trigger.cancel();
        assert!(follower.is_cancelled());
    }

    #[test]
    fn dropped_child_leaves_nothing_behind() {
        let parent = CancelToken::none();
        let (trigger, child) = parent.child();
        let child_state = Arc::downgrade(&child.0);
        drop((trigger, child));
        assert_eq!(child_state.strong_count(), 0);

        let _live = parent.child();
        assert_eq!(parent.0.children().len(), 1);
        assert!(!parent.is_cancelled());
    }

    #[tokio::test]
    async fn grandchild_cancelled_by_root() {
        let (root_trigger, root) = CancelToken::new();
        let (_child_trigger, child) = root.child();
        let (_grandchild_trigger, grandchild) = child.child();
        root_trigger.cancel();
        grandchild.cancelled().await;
        assert!(child.is_cancelled());
    }

    #[tokio::test]
    async fn drop_trigger_also_cancels() {
        let (trigger, token) = CancelToken::new();
        drop(trigger);
        token.cancelled().await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn race_returns_value_when_not_cancelled() {
        let (_trigger, token) = CancelToken::new();
        let result = token.race(async { 42 }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn race_returns_error_when_already_cancelled() {
        let (trigger, token) = CancelToken::new();
        trigger.cancel();
        let result = token.race(std::future::pending::<()>()).await;
        assert!(result.unwrap_err().contains("cancelled"));
    }

    #[tokio::test]
    async fn race_interrupted_by_concurrent_cancel() {
        let (trigger, token) = CancelToken::new();
        tokio::spawn(async move { trigger.cancel() });
        let result = token.race(std::future::pending::<()>()).await;
        assert!(result.is_err());
    }

    #[test]
    fn trigger_identity_tells_registrations_apart() {
        let (trigger, token) = CancelToken::new();
        let (_other_trigger, other_token) = CancelToken::new();
        assert!(trigger.fires(&token));
        assert!(!trigger.fires(&other_token));
    }

    #[test]
    fn unlink_stops_propagation_and_leaves_siblings_alone() {
        let (parent_trigger, parent) = CancelToken::new();
        let (_unlinked_trigger, unlinked) = parent.child();
        let (_grandchild_trigger, grandchild) = unlinked.child();
        let (_sibling_trigger, sibling) = parent.child();

        unlinked.unlink(&parent);
        assert_eq!(parent.0.children().len(), 1, "only the sibling is left");

        parent_trigger.cancel();
        assert!(sibling.is_cancelled());
        assert!(!unlinked.is_cancelled());
        assert!(!grandchild.is_cancelled());
    }

    /// `drained = false` is the window inside `fire` between raising the flag
    /// and draining the children: the unlink must not slip the child out.
    #[test_case(true  ; "after_the_parent_fired")]
    #[test_case(false ; "while_the_parent_fires")]
    fn unlink_under_a_cancelled_parent_cancels_the_child(drained: bool) {
        let (parent_trigger, parent) = CancelToken::new();
        let (_child_trigger, child) = parent.child();
        if drained {
            parent_trigger.cancel();
        } else {
            parent.0.cancelled.store(true, Ordering::Release);
        }

        child.unlink(&parent);
        assert!(child.is_cancelled());
    }

    const KEY: &str = "x";
    const OTHER_KEY: &str = "y";
    const LOST_CANCEL: &str = "the cancel left no mark for the session after it";

    fn key() -> String {
        KEY.to_owned()
    }

    /// What the id looks like when the cancel lands.
    enum Shape {
        /// Nothing registered yet, so the cancel and the first session race.
        Empty,
        Occupied,
        /// One session retired and the next has yet to register, the gap a tool
        /// call leaves between two it opens back to back.
        Hole,
    }

    /// Whatever shape the id is in, the cancel has to reach the sessions the
    /// tool call has not opened yet. Losing it left the next session running
    /// with its pane already marked cancelled.
    #[test_case(Shape::Empty    ; "the_cancel_beat_the_first_session")]
    #[test_case(Shape::Occupied ; "a_sibling_is_still_running")]
    #[test_case(Shape::Hole     ; "between_two_sessions_of_one_tool_call")]
    fn cancel_map_cancel_catches_the_session_that_registers_after_it(shape: Shape) {
        let map: CancelMap<String> = CancelMap::new();
        let (earlier, _token) = CancelToken::new();
        match shape {
            Shape::Empty => {}
            Shape::Occupied => {
                map.insert(key(), earlier);
            }
            Shape::Hole => {
                let slot = map.insert(key(), earlier);
                map.retire(&key(), slot);
            }
        }

        map.cancel(key());

        let (trigger, token) = CancelToken::new();
        map.insert(key(), trigger);
        assert!(token.is_cancelled(), "{LOST_CANCEL}");
    }

    #[test]
    fn cancel_map_cancel_all_stops_everything_and_forgets_the_marks() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        map.insert(key(), t1);
        map.insert(OTHER_KEY.to_owned(), t2);
        map.cancel(key());

        map.cancel_all();
        assert!(tok1.is_cancelled());
        assert!(tok2.is_cancelled());
        assert!(!map.has_key(&key()));

        let (trigger, token) = CancelToken::new();
        map.insert(key(), trigger);
        assert!(!token.is_cancelled());
    }

    /// One tool call can open several subagents. They used to evict each
    /// other, so the first died the moment the second registered.
    #[test]
    fn cancel_map_keeps_siblings_under_one_key() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        map.insert(key(), t1);
        map.insert(key(), t2);
        assert!(!tok1.is_cancelled(), "a sibling must not evict the first");
        assert!(!tok2.is_cancelled());

        map.cancel(key());
        assert!(tok1.is_cancelled(), "cancelling the key stops them all");
        assert!(tok2.is_cancelled());
    }

    #[test]
    fn cancel_map_retire_leaves_siblings_running() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        let slot1 = map.insert(key(), t1);
        map.insert(key(), t2);

        map.retire(&key(), slot1);
        assert!(tok1.is_cancelled(), "retiring drops that trigger");
        assert!(!tok2.is_cancelled(), "the sibling keeps running");

        map.cancel(key());
        assert!(tok2.is_cancelled());
    }

    /// After a reload, handles from the old instance can still retire their
    /// slots, and they must not touch the new ones.
    #[test]
    fn cancel_map_revive_survives_stale_retires_and_cancels_again() {
        let map = CancelMap::new();
        map.revive(&key());
        assert!(!map.has_key(&key()), "reviving an unknown id must not leak");

        let (stale_trigger, _stale) = CancelToken::new();
        let stale_slot = map.insert(key(), stale_trigger);
        map.cancel(key());
        map.revive(&key());
        let (trigger, token) = CancelToken::new();
        map.insert(key(), trigger);

        map.retire(&key(), stale_slot);
        assert!(
            !token.is_cancelled(),
            "a stale slot must not retire a new one"
        );

        map.cancel(key());
        assert!(token.is_cancelled(), "a revived id can be cancelled again");
    }

    #[test_case(false ; "registered_before_the_cancel")]
    #[test_case(true  ; "registered_under_a_cancelled_id")]
    fn cancel_map_cancel_fires_a_handle(cancel_first: bool) {
        let map = CancelMap::new();
        let (session_trigger, token) = CancelToken::new();
        if cancel_first {
            map.cancel(key());
        }
        map.insert_handle(key(), session_trigger.handle());
        assert_eq!(token.is_cancelled(), cancel_first);

        map.cancel(key());
        assert!(token.is_cancelled());
    }

    enum Forget {
        Retire,
        CancelAll,
        DropMap,
    }

    /// The run ending must not reach a session that moved to the background:
    /// only its own trigger ends it.
    #[test_case(Forget::Retire    ; "retire")]
    #[test_case(Forget::CancelAll ; "cancel_all")]
    #[test_case(Forget::DropMap   ; "drop_map")]
    fn cancel_map_forgets_a_handle_without_firing_it(forget: Forget) {
        let map = CancelMap::new();
        let (session_trigger, token) = CancelToken::new();
        let slot = map.insert_handle(key(), session_trigger.handle());
        match forget {
            Forget::Retire => map.retire(&key(), slot),
            Forget::CancelAll => map.cancel_all(),
            Forget::DropMap => drop(map),
        }
        assert!(!token.is_cancelled());
    }
}
