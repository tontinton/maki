//! Feeds monty's memory counters so `max_memory` holds in-process.
//!
//! monty v0.0.21 checks `LIVE_MEMORY - BASELINE_MEMORY` at VM checkpoints,
//! but something must feed those counters. Upstream feeds them with
//! `monty-alloc` in a throwaway worker process that dies on a breach. maki
//! runs the interpreter in-process, so this allocator does the accounting
//! instead: it charges allocations made by a thread holding a
//! [`SandboxScope`], and monty's tracker turns a breach into a plain
//! Python `MemoryError`.
//!
//! Being the `#[global_allocator]` of every binary that links this crate
//! (a second one anywhere fails to compile), enforcement can never go
//! silently missing. Outside a scope each allocation pays one thread-local
//! read and touches nothing shared.
//!
//! The counters are process-wide, so overlapping runs share one budget:
//! only the outermost rebases the baseline, and the tightest `max_memory`
//! wins. Serializing instead would deadlock, because a script suspended in
//! a `task` call holds its scope while the subagent it waits on starts a
//! run of its own.
//!
//! The accounting is approximate: tool results are allocated on other
//! threads (never charged) yet often freed inside the scope (refunded), so
//! a large result grants that much unearned headroom. Good enough as a
//! guardrail against runaway agent code, not a security boundary. Once
//! monty ships `set_memory_probe` (pydantic/monty#740) the counter becomes
//! thread-local and the sharing and rebasing go away.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;

use monty_types::{BASELINE_MEMORY, LIVE_MEMORY};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

thread_local! {
    static IN_SANDBOX: Cell<bool> = const { Cell::new(false) };
}

struct CountingAllocator;

fn in_sandbox() -> bool {
    IN_SANDBOX.try_with(Cell::get).unwrap_or(false)
}

fn charge(size: usize) {
    if in_sandbox() {
        LIVE_MEMORY.fetch_add(size, Relaxed);
    }
}

/// Saturating: the sandbox thread frees blocks it was never charged for
/// (allocated outside the scope), and wrapping below zero would read as a
/// huge live count and trip the limit instantly.
fn refund(size: usize) {
    if in_sandbox() {
        let _ = LIVE_MEMORY.fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(size)));
    }
}

// SAFETY: every method forwards to `System` unchanged; the accounting on
// the side touches no pointers.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        charge(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        charge(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        refund(layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if let Some(grown) = new_size.checked_sub(layout.size()) {
            charge(grown);
        } else {
            refund(layout.size() - new_size);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

static ACTIVE_SCOPES: AtomicUsize = AtomicUsize::new(0);

/// Marks the current thread as the interpreter's for the scope's lifetime.
/// The first live scope rebases the counters so the run starts from zero;
/// later ones join its budget rather than forgiving it their usage.
pub(crate) struct SandboxScope;

impl SandboxScope {
    pub(crate) fn enter() -> Self {
        if ACTIVE_SCOPES.fetch_add(1, Relaxed) == 0 {
            BASELINE_MEMORY.store(LIVE_MEMORY.load(Relaxed), Relaxed);
        }
        IN_SANDBOX.set(true);
        SandboxScope
    }
}

impl Drop for SandboxScope {
    fn drop(&mut self) {
        IN_SANDBOX.set(false);
        ACTIVE_SCOPES.fetch_sub(1, Relaxed);
    }
}
