//! Cooperative cancellation with parent-to-child propagation.
//!
//! `CancelTrigger` fires on Drop, so cleanup happens even if the trigger is forgotten.
//! `cancelled()` uses a double-check around the listener to close the TOCTOU window between flag read and listener registration.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
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

enum Entry {
    Live(#[allow(dead_code)] CancelTrigger),
    PreCancelled,
}

pub struct CancelMap<K> {
    entries: Mutex<HashMap<K, Entry>>,
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
        }
    }

    pub fn insert(&self, id: K, trigger: CancelTrigger) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match map.remove(&id) {
            Some(Entry::PreCancelled) => drop(trigger),
            _ => {
                map.insert(id, Entry::Live(trigger));
            }
        }
    }

    pub fn cancel_or_precancel(&self, id: K) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match map.remove(&id) {
            Some(Entry::Live(_)) => {} // trigger dropped, fires cancel
            _ => {
                map.insert(id, Entry::PreCancelled);
            }
        }
    }

    pub fn remove(&self, id: &K) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
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
    fn cancel_map_remove_clears_precancelled() {
        let map: CancelMap<String> = CancelMap::new();
        map.cancel_or_precancel("t1".to_owned());
        map.remove(&"t1".to_owned());
        let (trigger, token) = CancelToken::new();
        map.insert("t1".to_owned(), trigger);
        assert!(!token.is_cancelled(), "remove should clear PreCancelled");
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
    fn cancel_map_cancel_all_clears_precancelled() {
        let map: CancelMap<String> = CancelMap::new();
        map.cancel_or_precancel("t1".to_owned());
        map.cancel_all();
        let (trigger, token) = CancelToken::new();
        map.insert("t1".to_owned(), trigger);
        assert!(
            !token.is_cancelled(),
            "cancel_all should clear PreCancelled entries"
        );
    }

    #[test]
    fn cancel_map_insert_overwrites_live() {
        let map = CancelMap::new();
        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        map.insert("x".to_owned(), t1);
        map.insert("x".to_owned(), t2);
        assert!(
            tok1.is_cancelled(),
            "first trigger should be dropped on overwrite"
        );
        assert!(!tok2.is_cancelled(), "second trigger should remain live");
        map.cancel_or_precancel("x".to_owned());
        assert!(tok2.is_cancelled());
    }
}
