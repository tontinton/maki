use std::time::Duration;

const BASE_DELAY: Duration = Duration::from_secs(2);
const MAX_DELAY: Duration = Duration::from_secs(32);
pub const MAX_TIMEOUT_RETRIES: u32 = 10;

#[derive(Default)]
pub struct RetryState {
    attempt: u32,
}

impl RetryState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next_delay(&mut self) -> (u32, Duration) {
        self.attempt += 1;
        let delay = BASE_DELAY.saturating_mul(1 << self.attempt.min(5)).min(MAX_DELAY);
        (self.attempt, delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(1, 4  ; "first_attempt")]
    #[test_case(2, 8  ; "second_attempt")]
    #[test_case(3, 16 ; "third_attempt")]
    #[test_case(4, 32 ; "fourth_hits_max")]
    #[test_case(5, 32 ; "fifth_stays_at_max")]
    #[test_case(6, 32 ; "sixth_stays_at_max")]
    fn delay_progression(calls: u32, expected_secs: u64) {
        let mut state = RetryState::new();
        let mut delay = Duration::ZERO;
        for _ in 0..calls {
            (_, delay) = state.next_delay();
        }
        assert_eq!(delay, Duration::from_secs(expected_secs));
    }
}
