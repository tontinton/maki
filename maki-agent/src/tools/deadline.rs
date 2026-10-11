//! The time a call has to finish by. Waiting on the user, like a permission
//! prompt, doesn't count as running, so [`Deadline::paused_while`] moves the
//! deadline later by however long the wait took.
//!
//! All clones share one instant. The output hook chain holds a copy of the
//! context made before the prompt, and it still needs to see the new time.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

pub(crate) const DEADLINE_EXCEEDED: &str = "timeout exceeded";

#[derive(Clone, Debug, Default)]
pub struct Deadline(Option<Arc<Mutex<Instant>>>);

impl Deadline {
    pub fn after(budget: Duration) -> Self {
        Self(Some(Arc::new(Mutex::new(Instant::now() + budget))))
    }

    fn instant(&self) -> Option<MutexGuard<'_, Instant>> {
        self.0
            .as_deref()
            .map(|at| at.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn at(&self) -> Option<Instant> {
        self.instant().as_deref().copied()
    }

    /// Only wrap waits on the user. Real work should still spend the budget.
    /// A wait dropped halfway moves nothing, and that's fine because its call
    /// is gone too.
    pub async fn paused_while<F: Future>(&self, wait: F) -> F::Output {
        let paused_at = Instant::now();
        let output = wait.await;
        if let Some(mut at) = self.instant() {
            *at += paused_at.elapsed();
        }
        output
    }

    fn remaining(&self) -> Option<Duration> {
        self.at()
            .map(|at| at.saturating_duration_since(Instant::now()))
    }

    pub fn check(&self) -> Result<(), String> {
        match self.remaining() {
            Some(left) if left.is_zero() => Err(DEADLINE_EXCEEDED.into()),
            _ => Ok(()),
        }
    }

    pub fn cap_timeout(&self, timeout_secs: u64) -> Result<u64, String> {
        match self.remaining() {
            None => Ok(timeout_secs),
            Some(left) if left.is_zero() => Err(DEADLINE_EXCEEDED.into()),
            Some(left) => Ok(timeout_secs.min(left.as_secs().max(1))),
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const BUDGET: Duration = Duration::from_secs(60);
    const WAIT: Duration = Duration::from_millis(20);

    #[test_case(Deadline::default(), 120, 120 ; "none_passes_through")]
    #[test_case(Deadline::after(BUDGET), 10, 10 ; "requested_under_remaining")]
    fn cap_timeout_ok(deadline: Deadline, requested: u64, expected: u64) {
        assert_eq!(deadline.cap_timeout(requested).unwrap(), expected);
    }

    #[test]
    fn cap_timeout_clamps_to_remaining() {
        let clamped = Deadline::after(BUDGET).cap_timeout(7200).unwrap();
        assert!(clamped <= BUDGET.as_secs(), "got {clamped}");
    }

    #[test]
    fn expired_deadline_is_rejected() {
        let expired = Deadline::after(Duration::ZERO);
        assert_eq!(expired.cap_timeout(120).unwrap_err(), DEADLINE_EXCEEDED);
        assert_eq!(expired.check().unwrap_err(), DEADLINE_EXCEEDED);
    }

    #[test]
    fn a_wait_pushes_every_clone_back_by_its_length() {
        let deadline = Deadline::after(BUDGET);
        let before = deadline.at().unwrap();
        let clone = deadline.clone();

        smol::block_on(deadline.paused_while(smol::Timer::after(WAIT)));

        assert!(clone.at().unwrap() - before >= WAIT);
    }
}
