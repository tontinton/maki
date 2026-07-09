//! Cooperative cancel signal for provider SSE loops.
//!
//! Lives in `maki-providers` because the `Provider::stream_message` trait
//! and its SSE parsers are here; `maki-agent` owns the higher-level
//! `CancelToken` and adapts it via [`CancellationToken::from_flag`].
//!
//! The SSE loop polls [`CancellationToken::is_cancelled`] between lines and
//! breaks early so the parser can return a partial `StreamResponse` with
//! whatever blocks were accumulated (including signed thinking, §8).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// A token that is never cancelled (used by providers/tests that don't opt in).
    pub fn never() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Wrap an existing `AtomicBool` flag shared with a higher-level cancel source.
    pub fn from_flag(cancelled: Arc<AtomicBool>) -> Self {
        Self { cancelled }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}
