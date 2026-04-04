use std::collections::HashMap;

#[cfg(test)]
use maki_agent::CancelToken;
use maki_agent::CancelTrigger;

enum Entry {
    Live(CancelTrigger),
    PreCancelled,
}

pub(super) struct CancelMap {
    entries: HashMap<u64, Entry>,
}

impl CancelMap {
    pub(super) fn new(run_id: u64, trigger: CancelTrigger) -> Self {
        Self {
            entries: HashMap::from([(run_id, Entry::Live(trigger))]),
        }
    }

    pub(super) fn insert(&mut self, run_id: u64, trigger: CancelTrigger) {
        match self.entries.remove(&run_id) {
            Some(Entry::PreCancelled) => trigger.cancel(),
            Some(Entry::Live(_)) | None => {
                self.entries.insert(run_id, Entry::Live(trigger));
            }
        }
    }

    pub(super) fn cancel(&mut self, run_id: u64) {
        match self.entries.remove(&run_id) {
            Some(Entry::Live(trigger)) => trigger.cancel(),
            Some(Entry::PreCancelled) | None => {
                self.entries.insert(run_id, Entry::PreCancelled);
            }
        }
    }

    pub(super) fn cancel_all(&mut self) {
        for (_, entry) in self.entries.drain() {
            if let Entry::Live(trigger) = entry {
                trigger.cancel();
            }
        }
    }

    pub(super) fn remove(&mut self, run_id: u64) {
        self.entries.remove(&run_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_before_insert_fires_on_insert() {
        let (trigger, token) = CancelToken::new();
        let mut map = CancelMap::new(99, trigger);
        map.cancel(1);
        assert!(!token.is_cancelled());

        let (trigger2, token2) = CancelToken::new();
        map.insert(1, trigger2);
        assert!(token2.is_cancelled());
    }

    #[test]
    fn cancel_all_fires_live_and_clears_map() {
        let (t1, tok1) = CancelToken::new();
        let (t2, tok2) = CancelToken::new();
        let mut map = CancelMap::new(0, t1);
        map.insert(1, t2);
        map.cancel(5);

        map.cancel_all();
        assert!(tok1.is_cancelled());
        assert!(tok2.is_cancelled());

        let (t3, tok3) = CancelToken::new();
        map.insert(5, t3);
        assert!(
            !tok3.is_cancelled(),
            "cancel_all should clear PreCancelled entries"
        );
    }

    #[test]
    fn cancel_does_not_affect_other_runs() {
        let (t0, tok0) = CancelToken::new();
        let (t1, tok1) = CancelToken::new();
        let mut map = CancelMap::new(0, t0);
        map.insert(1, t1);
        map.cancel(1);
        assert!(tok1.is_cancelled());
        assert!(!tok0.is_cancelled());
    }

    #[test]
    fn reused_id_after_cancel_gets_clean_slot() {
        let (t1, tok1) = CancelToken::new();
        let mut map = CancelMap::new(0, t1);
        map.cancel(0);
        assert!(tok1.is_cancelled());

        let (t2, tok2) = CancelToken::new();
        map.insert(0, t2);
        assert!(!tok2.is_cancelled());
    }

    #[test]
    fn remove_prevents_precancelled_from_firing() {
        let (trigger, _token) = CancelToken::new();
        let mut map = CancelMap::new(0, trigger);
        map.cancel(5);
        map.remove(5);

        let (t2, tok2) = CancelToken::new();
        map.insert(5, t2);
        assert!(!tok2.is_cancelled());
    }
}
