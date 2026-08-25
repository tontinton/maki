use std::time::Duration;

const DELAY: Duration = Duration::from_secs(2);
const MAX_DELAY: Duration = Duration::from_secs(8);
pub const MAX_TIMEOUT_RETRIES: u32 = 10;

#[derive(Default)]
pub struct RetryState {
    attempt: u32,
}

impl RetryState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Retries burned so far; the attempt currently failing is this plus one.
    pub fn attempts(&self) -> u32 {
        self.attempt
    }

    pub fn next_delay(&mut self) -> (u32, Duration) {
        self.attempt += 1;
        let delay = (DELAY.saturating_mul(self.attempt)).min(MAX_DELAY);
        let half = delay / 2;
        let jitter = Duration::from_millis(fastrand::u64(0..=half.as_millis() as u64));
        (self.attempt, half + jitter)
    }
}
