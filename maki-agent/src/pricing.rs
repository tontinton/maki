use std::ops::AddAssign;

const PER_MILLION: f64 = 1_000_000.0;

#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

pub const SONNET_4: ModelPricing = ModelPricing {
    input: 3.0,
    output: 15.0,
    cache_write: 3.75,
    cache_read: 0.30,
};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct TokenUsage {
    pub input: u32,
    pub output: u32,
    pub cache_creation: u32,
    pub cache_read: u32,
}

impl TokenUsage {
    pub fn cost(&self, pricing: &ModelPricing) -> f64 {
        self.input as f64 * pricing.input / PER_MILLION
            + self.output as f64 * pricing.output / PER_MILLION
            + self.cache_creation as f64 * pricing.cache_write / PER_MILLION
            + self.cache_read as f64 * pricing.cache_read / PER_MILLION
    }
}

impl AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input += rhs.input;
        self.output += rhs.output;
        self.cache_creation += rhs.cache_creation;
        self.cache_read += rhs.cache_read;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_cost() {
        let usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_creation: 1_000_000,
            cache_read: 1_000_000,
        };
        let expected = 3.0 + 15.0 + 3.75 + 0.30;
        assert!((usage.cost(&SONNET_4) - expected).abs() < 1e-9);
    }
}
