//! Deduplicates the large repeated strings a session file carries, for the
//! duration of one load and no longer.
//!
//! A session records a tool's output and the message that carries it as
//! separate records, each with its own copy of any base64 image payload, so
//! loading one decodes every image twice. Interning collapses the copies.
//!
//! The scope is the point. `Arc<str>` stores its bytes in the same allocation
//! as its refcounts, so a table that outlived the load would keep whole
//! payloads resident long after the session dropped them. A `Weak` does not
//! avoid that: the last *weak* release is what frees the allocation, and `str`
//! has no destructor to run before it, so a dead weak entry still holds every
//! byte it ever pointed at.

use std::cell::RefCell;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::Arc;

thread_local! {
    /// `None` outside a load, so nothing is interned and nothing is retained.
    static TABLE: RefCell<Option<HashSet<Arc<str>>>> = const { RefCell::new(None) };
}

/// Enables interning on the current thread until dropped.
///
/// The raw pointer makes this `!Send`, which is what confines the scope to one
/// thread. Dropped on any other, it would clear that thread's empty table and
/// leave the entering thread interning for the rest of the process, retaining
/// every payload it ever saw.
pub struct Scope(PhantomData<*const ()>);

impl Scope {
    pub fn enter() -> Self {
        TABLE.with_borrow_mut(|table| *table = Some(HashSet::new()));
        Self(PhantomData)
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        TABLE.with_borrow_mut(|table| *table = None);
    }
}

/// A shared handle to `data`, reusing an identical payload already seen in this
/// scope. Outside a scope this is a plain allocation.
pub fn shared_str(data: String) -> Arc<str> {
    TABLE.with_borrow_mut(|table| {
        let Some(table) = table.as_mut() else {
            return Arc::from(data);
        };
        if let Some(hit) = table.get(data.as_str()) {
            return Arc::clone(hit);
        }
        let shared: Arc<str> = Arc::from(data);
        table.insert(Arc::clone(&shared));
        shared
    })
}

#[cfg(test)]
mod tests {
    use super::{Scope, shared_str};
    use std::sync::Arc;

    const PAYLOAD: &str = "aW50ZXJuZWQtcGF5bG9hZA==";
    const OTHER: &str = "b3RoZXItcGF5bG9hZA==";

    /// Counting references rather than probing a `Weak`: an upgrade failing
    /// only proves the value was dropped, and the bytes of an `Arc<str>` outlive
    /// that. A strong count of 1 is what proves the table let go.
    #[test]
    fn scope_exit_releases_every_payload_it_interned() {
        let kept = {
            let _scope = Scope::enter();
            let first = shared_str(PAYLOAD.to_owned());
            let second = shared_str(PAYLOAD.to_owned());
            let other = shared_str(OTHER.to_owned());

            assert!(
                Arc::ptr_eq(&first, &second),
                "a repeat must reuse the first"
            );
            assert!(
                !Arc::ptr_eq(&first, &other),
                "distinct payloads stay distinct"
            );
            assert_eq!(&*first, PAYLOAD);
            assert_eq!(
                Arc::strong_count(&first),
                3,
                "both handles plus the table's own"
            );
            drop(second);
            first
        };

        assert_eq!(
            Arc::strong_count(&kept),
            1,
            "the table must not outlive the load that filled it"
        );
    }

    #[test]
    fn outside_a_scope_nothing_is_interned() {
        let first = shared_str(PAYLOAD.to_owned());
        let second = shared_str(PAYLOAD.to_owned());

        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(Arc::strong_count(&first), 1);
    }

    #[test]
    fn interning_stops_when_the_scope_ends() {
        let interned = {
            let _scope = Scope::enter();
            shared_str(PAYLOAD.to_owned())
        };

        assert!(!Arc::ptr_eq(&interned, &shared_str(PAYLOAD.to_owned())));
    }
}
