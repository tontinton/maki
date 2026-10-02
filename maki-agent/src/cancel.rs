//! Cooperative cancellation with parent-to-child propagation.
//!
//! `CancelTrigger` fires on Drop, so cleanup happens even if the trigger is forgotten.
//! `cancelled()` uses a double-check around the listener to close the TOCTOU window between flag read and listener registration.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use event_listener::Event;

struct Shared {
    cancelled: AtomicBool,
    event: Event,
}

impl Shared {
    fn fire(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.event.notify(usize::MAX);
    }
}

#[derive(Clone)]
pub struct CancelToken(Arc<Shared>);

pub struct CancelTrigger(Arc<Shared>);

impl CancelToken {
    pub fn new() -> (CancelTrigger, Self) {
        let shared = Arc::new(Shared {
            cancelled: AtomicBool::new(false),
            event: Event::new(),
        });
        (CancelTrigger(Arc::clone(&shared)), Self(shared))
    }

    pub fn none() -> Self {
        Self(Arc::new(Shared {
            cancelled: AtomicBool::new(false),
            event: Event::new(),
        }))
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub async fn race<T>(&self, future: impl Future<Output = T>) -> Result<T, String> {
        if self.is_cancelled() {
            return Err("cancelled".into());
        }
        futures_lite::future::race(async { Ok(future.await) }, async {
            self.cancelled().await;
            Err("cancelled".into())
        })
        .await
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
        let parent = self.clone();
        let child_shared = Arc::clone(&child_token.0);
        smol::spawn(async move {
            parent.cancelled().await;
            child_shared.fire();
        })
        .detach();
        (child_trigger, child_token)
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

#[derive(Default)]
struct Entry {
    /// Held, never read. Dropping a trigger is what fires the token of the
    /// session that registered it.
    registrations: Vec<Registration>,
    cancelled: bool,
}

struct Registration {
    slot: CancelSlot,
    /// Held, never read. Dropping it is what fires the token.
    #[allow(dead_code)]
    trigger: CancelTrigger,
    /// Detached (background) sessions outlive the run that spawned them, so
    /// the run-end sweep must not fire their triggers.
    detached: bool,
}

/// Triggers grouped under an id a user can name and stop from the outside. One
/// subagent tool call can open several sessions under a single `tool_use_id`,
/// and cancelling that id has to reach all of them, even the ones opened after
/// the cancel.
///
/// So the mark lives on the id until the whole map is drained by
/// [`cancel_all`](Self::cancel_all). Clearing it when the last sibling retires
/// would lose the cancels that land while the id sits empty, which it does
/// before the first session registers and again between two sessions one tool
/// call opens back to back.
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
    /// returns the slot to hand back to [`retire`](Self::retire). {detached}
    /// marks the session as outliving the run, excluded from
    /// [`end_run`](Self::end_run).
    pub fn insert(&self, id: K, trigger: CancelTrigger, detached: bool) -> CancelSlot {
        let mut map = self.lock();
        let slot = CancelSlot(self.next_slot.fetch_add(1, Ordering::Relaxed));
        let entry = map.entry(id).or_default();
        // Under a cancelled id the trigger is dropped instead of stored, and
        // that drop is what fires the token, so the session is born cancelled.
        if !entry.cancelled {
            entry.registrations.push(Registration {
                slot,
                trigger,
                detached,
            });
        }
        slot
    }

    /// Retires one registration and drops its trigger. Siblings and the id's
    /// cancelled mark stay.
    pub fn retire(&self, id: &K, slot: CancelSlot) {
        let mut map = self.lock();
        let Some(entry) = map.get_mut(id) else {
            return;
        };
        entry
            .registrations
            .retain(|registered| registered.slot != slot);
    }

    /// Cancels every registration under {id} and marks the id, so a session
    /// registering under it later is born cancelled too. Returns whether any
    /// live registration was cancelled; `false` is a pure precancel, i.e. the
    /// caller addressed a session that does not exist.
    pub fn cancel(&self, id: K) -> bool {
        let mut map = self.lock();
        let entry = map.entry(id).or_default();
        entry.cancelled = true;
        let hit = !entry.registrations.is_empty();
        entry.registrations.clear();
        hit
    }

    /// The run that registered these is over: stop the sessions still attached
    /// to it and drop their ids with it, so no cancel of this run leaks into
    /// the next one. Detached registrations survive — a background session
    /// must outlive the turn that spawned it, stopped only by a targeted
    /// cancel or [`cancel_all`](Self::cancel_all).
    pub fn end_run(&self) {
        self.lock().retain(|_, entry| {
            entry.registrations.retain(|registered| registered.detached);
            !entry.registrations.is_empty() || entry.cancelled
        });
    }

    /// Cancels every registration and drops the marks, so nothing survives a
    /// respawn or a user cancel-all.
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

    #[test]
    fn trigger_wakes_token() {
        smol::block_on(async {
            let (trigger, token) = CancelToken::new();
            assert!(!token.is_cancelled());
            trigger.cancel();
            token.cancelled().await;
            assert!(token.is_cancelled());
        });
    }

    #[test]
    fn child_cancelled_by_parent() {
        smol::block_on(async {
            let (parent_trigger, parent_token) = CancelToken::new();
            let (_child_trigger, child_token) = parent_token.child();
            parent_trigger.cancel();
            child_token.cancelled().await;
            assert!(child_token.is_cancelled());
        });
    }

    #[test]
    fn child_cancelled_by_own_trigger() {
        smol::block_on(async {
            let (_parent_trigger, parent_token) = CancelToken::new();
            let (child_trigger, child_token) = parent_token.child();
            child_trigger.cancel();
            child_token.cancelled().await;
            assert!(child_token.is_cancelled());
            assert!(!parent_token.is_cancelled());
        });
    }

    #[test]
    fn drop_trigger_also_cancels() {
        smol::block_on(async {
            let (trigger, token) = CancelToken::new();
            drop(trigger);
            token.cancelled().await;
            assert!(token.is_cancelled());
        });
    }

    #[test]
    fn race_returns_value_when_not_cancelled() {
        smol::block_on(async {
            let (_trigger, token) = CancelToken::new();
            let result = token.race(async { 42 }).await;
            assert_eq!(result.unwrap(), 42);
        });
    }

    #[test]
    fn race_returns_error_when_already_cancelled() {
        smol::block_on(async {
            let (trigger, token) = CancelToken::new();
            trigger.cancel();
            let result = token.race(std::future::pending::<()>()).await;
            assert!(result.unwrap_err().contains("cancelled"));
        });
    }

    #[test]
    fn race_interrupted_by_concurrent_cancel() {
        smol::block_on(async {
            let (trigger, token) = CancelToken::new();
            smol::spawn(async move { trigger.cancel() }).detach();
            let result = token.race(std::future::pending::<()>()).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn trigger_identity_tells_registrations_apart() {
        let (trigger, token) = CancelToken::new();
        let (_other_trigger, other_token) = CancelToken::new();
        assert!(trigger.fires(&token));
        assert!(!trigger.fires(&other_token));
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
                map.insert(key(), earlier, false);
            }
            Shape::Hole => {
                let slot = map.insert(key(), earlier, false);
                map.retire(&key(), slot);
            }
        }

        map.cancel(key());

        let (trigger, token) = CancelToken::new();
        map.insert(key(), trigger, false);
        assert!(token.is_cancelled(), "{LOST_CANCEL}");
    }

    #[test]
    fn cancel_map_cancel_all_stops_everything_and_forgets_the_marks() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        map.insert(key(), t1, false);
        map.insert(OTHER_KEY.to_owned(), t2, false);
        map.cancel(key());

        map.cancel_all();
        assert!(tok1.is_cancelled());
        assert!(tok2.is_cancelled());
        assert!(!map.has_key(&key()));

        let (trigger, token) = CancelToken::new();
        map.insert(key(), trigger, false);
        assert!(!token.is_cancelled());
    }

    /// One tool call can open several subagents. They used to evict each
    /// other, so the first died the moment the second registered.
    #[test]
    fn cancel_map_keeps_siblings_under_one_key() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        map.insert(key(), t1, false);
        map.insert(key(), t2, false);
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
        let slot1 = map.insert(key(), t1, false);
        map.insert(key(), t2, false);

        map.retire(&key(), slot1);
        assert!(tok1.is_cancelled(), "retiring drops that trigger");
        assert!(!tok2.is_cancelled(), "the sibling keeps running");

        map.cancel(key());
        assert!(tok2.is_cancelled());
    }

    /// A detached session outlives the run that spawned it: the run-end sweep
    /// stops only what is still attached. Losing this killed background
    /// subagents the moment the parent's turn ended.
    #[test]
    fn end_run_stops_attached_but_spares_detached() {
        let map = CancelMap::new();
        let (attached, attached_tok) = CancelToken::new();
        let (detached, detached_tok) = CancelToken::new();
        map.insert(key(), attached, false);
        map.insert(key(), detached, true);

        map.end_run();
        assert!(
            attached_tok.is_cancelled(),
            "the run-end sweep stops attached sessions"
        );
        assert!(
            !detached_tok.is_cancelled(),
            "the run-end sweep must not kill detached sessions"
        );

        // A targeted cancel still reaches the survivor.
        map.cancel(key());
        assert!(detached_tok.is_cancelled());
    }

    #[test]
    fn end_run_forgets_swept_ids_but_keeps_detached_marks() {
        let map = CancelMap::new();
        let (t1, _tok1) = CancelToken::new();
        let (t2, _tok2) = CancelToken::new();
        map.insert(key(), t1, false);
        map.insert(OTHER_KEY.to_owned(), t2, true);

        map.end_run();
        assert!(
            !map.has_key(&key()),
            "an id with nothing left must not leak"
        );
        assert!(
            map.has_key(&OTHER_KEY.to_owned()),
            "a detached session keeps its id"
        );

        // The cancelled mark survives while the id lives on.
        map.cancel(OTHER_KEY.to_owned());
        let (trigger, token) = CancelToken::new();
        map.insert(OTHER_KEY.to_owned(), trigger, true);
        assert!(token.is_cancelled(), "{LOST_CANCEL}");
    }
}
