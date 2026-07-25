//! Cooperative cancellation with parent-to-child propagation.
//!
//! `CancelTrigger` fires on Drop, so cleanup happens even if the trigger is forgotten.
//! `cancelled()` uses a double-check around the listener to close the TOCTOU window between flag read and listener registration.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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

struct Slotted {
    slot: CancelSlot,
    trigger: Option<CancelTrigger>,
}

#[derive(Default)]
struct Entry {
    registrations: Vec<Slotted>,
    cancelled: bool,
}

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
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let slot = CancelSlot(self.next_slot.fetch_add(1, Ordering::Relaxed));
        let entry = map.entry(id).or_default();
        let trigger = if entry.cancelled {
            drop(trigger);
            None
        } else {
            Some(trigger)
        };
        entry.registrations.push(Slotted { slot, trigger });
        slot
    }

    /// Retires one registration, dropping its trigger when it is still
    /// active and leaving its siblings alone.
    pub fn retire(&self, id: &K, slot: CancelSlot) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = map.get_mut(id) else {
            return;
        };
        entry
            .registrations
            .retain(|registration| registration.slot != slot);
        if entry.registrations.is_empty() {
            map.remove(id);
        }
    }

    /// Cancels everything under {id} and marks later siblings cancelled.
    /// The entry stays until every registered sibling retires.
    pub fn cancel_or_precancel(&self, id: K) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(id).or_default();
        entry.cancelled = true;
        for registration in &mut entry.registrations {
            drop(registration.trigger.take());
        }
    }

    pub fn remove(&self, id: &K) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    #[cfg(test)]
    fn has_key(&self, id: &K) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
    }

    pub fn cancel_all(&self) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain();
    }
}

#[cfg(test)]
mod tests {
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
    fn cancel_map_insert_and_cancel() {
        let map = CancelMap::new();
        let (trigger, token) = CancelToken::new();
        map.insert("t1".to_owned(), trigger);
        assert!(!token.is_cancelled());
        map.cancel_or_precancel("t1".to_owned());
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_map_cancel_before_insert() {
        let map = CancelMap::new();
        map.cancel_or_precancel("t1".to_owned());
        let (trigger, token) = CancelToken::new();
        map.insert("t1".to_owned(), trigger);
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_map_remove_clears_cancelled() {
        let map: CancelMap<String> = CancelMap::new();
        map.cancel_or_precancel("t1".to_owned());
        map.remove(&"t1".to_owned());
        let (trigger, token) = CancelToken::new();
        map.insert("t1".to_owned(), trigger);
        assert!(!token.is_cancelled(), "remove should clear cancellation");
    }

    #[test]
    fn cancel_map_cancel_all() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        map.insert("a".to_owned(), t1);
        map.insert("b".to_owned(), t2);
        map.cancel_all();
        assert!(tok1.is_cancelled());
        assert!(tok2.is_cancelled());
    }

    #[test]
    fn cancel_map_cancel_all_clears_cancelled() {
        let map: CancelMap<String> = CancelMap::new();
        map.cancel_or_precancel("t1".to_owned());
        map.cancel_all();
        let (trigger, token) = CancelToken::new();
        map.insert("t1".to_owned(), trigger);
        assert!(
            !token.is_cancelled(),
            "cancel_all should clear cancelled entries"
        );
    }

    /// One tool call can open several subagents. They used to evict each
    /// other, so the first died the moment the second registered.
    #[test]
    fn cancel_map_keeps_siblings_under_one_key() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        map.insert("x".to_owned(), t1);
        map.insert("x".to_owned(), t2);
        assert!(!tok1.is_cancelled(), "a sibling must not evict the first");
        assert!(!tok2.is_cancelled());

        map.cancel_or_precancel("x".to_owned());
        assert!(tok1.is_cancelled(), "cancelling the key stops them all");
        assert!(tok2.is_cancelled());
    }

    #[test]
    fn cancel_map_retire_leaves_siblings_running() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        let slot1 = map.insert("x".to_owned(), t1);
        map.insert("x".to_owned(), t2);

        map.retire(&"x".to_owned(), slot1);
        assert!(tok1.is_cancelled(), "retiring drops that trigger");
        assert!(!tok2.is_cancelled(), "the sibling keeps running");

        map.cancel_or_precancel("x".to_owned());
        assert!(tok2.is_cancelled());
    }

    /// The last one out clears the key so it can be reused.
    #[test]
    fn cancel_map_retiring_the_last_registration_clears_the_key() {
        let map = CancelMap::new();
        let (t1, _tok1) = CancelToken::new();
        let slot = map.insert("x".to_owned(), t1);
        assert!(map.has_key(&"x".to_owned()));

        map.retire(&"x".to_owned(), slot);
        assert!(!map.has_key(&"x".to_owned()), "empty key must be dropped");
    }

    /// Cancelling before anything registers has to catch every session the
    /// tool call goes on to open, not just the first one through the door.
    #[test]
    fn cancel_map_precancel_catches_every_later_sibling() {
        let map: CancelMap<String> = CancelMap::new();
        map.cancel_or_precancel("x".to_owned());

        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        let slot1 = map.insert("x".to_owned(), t1);
        let slot2 = map.insert("x".to_owned(), t2);
        assert!(tok1.is_cancelled());
        assert!(
            tok2.is_cancelled(),
            "the mark must outlive the first insert"
        );

        map.retire(&"x".to_owned(), slot1);
        assert!(map.has_key(&"x".to_owned()));
        map.retire(&"x".to_owned(), slot2);
        assert!(!map.has_key(&"x".to_owned()));
    }

    /// Pressing esc while a fan-out is running must also stop the sibling
    /// that starts a moment later.
    #[test]
    fn cancel_map_cancel_catches_a_sibling_registered_after() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let slot1 = map.insert("x".to_owned(), t1);

        map.cancel_or_precancel("x".to_owned());
        assert!(tok1.is_cancelled());

        let (t2, tok2) = CancelToken::new();
        let slot2 = map.insert("x".to_owned(), t2);
        assert!(tok2.is_cancelled(), "cancel left no mark for the sibling");

        map.retire(&"x".to_owned(), slot1);
        assert!(map.has_key(&"x".to_owned()));
        map.retire(&"x".to_owned(), slot2);
        assert!(!map.has_key(&"x".to_owned()));

        let (t3, tok3) = CancelToken::new();
        map.insert("x".to_owned(), t3);
        assert!(
            !tok3.is_cancelled(),
            "the completed call must not poison a reused tool id"
        );
    }

    #[test]
    fn cancel_map_insert_into_cancelled_returns_retirement_slot() {
        let map = CancelMap::new();
        map.cancel_or_precancel("x".to_owned());
        let (trigger, token) = CancelToken::new();
        let slot = map.insert("x".to_owned(), trigger);
        assert!(token.is_cancelled());
        map.retire(&"x".to_owned(), slot);
        assert!(!map.has_key(&"x".to_owned()));
    }
}
