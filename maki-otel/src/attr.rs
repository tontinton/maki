//! Sorted key/value lists that double as the identity of a metric stream.

use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Double(f64),
    Bool(bool),
}

impl Eq for AttrValue {}

impl Hash for AttrValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Str(s) => s.hash(state),
            Self::Int(v) => v.hash(state),
            Self::Double(v) => v.to_bits().hash(state),
            Self::Bool(v) => v.hash(state),
        }
    }
}

impl From<&str> for AttrValue {
    fn from(v: &str) -> Self {
        Self::Str(v.to_string())
    }
}

impl From<String> for AttrValue {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}

impl From<i64> for AttrValue {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<u64> for AttrValue {
    fn from(v: u64) -> Self {
        Self::Int(v as i64)
    }
}

impl From<u32> for AttrValue {
    fn from(v: u32) -> Self {
        Self::Int(i64::from(v))
    }
}

impl From<usize> for AttrValue {
    fn from(v: usize) -> Self {
        Self::Int(v as i64)
    }
}

impl From<f64> for AttrValue {
    fn from(v: f64) -> Self {
        Self::Double(v)
    }
}

impl From<bool> for AttrValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

/// Sorted by key with one entry per key, so two sets built in different orders
/// hash and compare the same and never split a metric into two streams.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct AttrSet(Vec<(String, AttrValue)>);

impl AttrSet {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<AttrValue>) {
        let key = key.into();
        let value = value.into();
        match self.0.binary_search_by(|(k, _)| k.as_str().cmp(&key)) {
            Ok(at) => self.0[at].1 = value,
            Err(at) => self.0.insert(at, (key, value)),
        }
    }

    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<AttrValue>) -> Self {
        self.insert(key, value);
        self
    }

    #[must_use]
    pub fn with_opt<V: Into<AttrValue>>(mut self, key: &str, value: Option<V>) -> Self {
        if let Some(value) = value {
            self.insert(key, value);
        }
        self
    }

    pub fn extend_from(&mut self, other: &AttrSet) {
        for (k, v) in &other.0 {
            self.insert(k.clone(), v.clone());
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &AttrValue)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;

    use super::*;

    const KEY_A: &str = "a";
    const KEY_B: &str = "b";

    fn count(set: &AttrSet) -> usize {
        set.iter().count()
    }

    fn hash(set: &AttrSet) -> u64 {
        let mut hasher = DefaultHasher::new();
        set.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn insertion_order_does_not_change_identity() {
        let one = AttrSet::new().with(KEY_A, 1i64).with(KEY_B, "x");
        let two = AttrSet::new().with(KEY_B, "x").with(KEY_A, 1i64);
        assert_eq!(one, two);
        assert_eq!(hash(&one), hash(&two));
    }

    #[test]
    fn repeated_key_replaces_instead_of_duplicating() {
        let set = AttrSet::new().with(KEY_A, 1i64).with(KEY_A, 2i64);
        assert_eq!(count(&set), 1);
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            vec![(KEY_A, &AttrValue::Int(2))]
        );
    }

    #[test]
    fn iteration_is_sorted_by_key() {
        let set = AttrSet::new()
            .with("z", 1i64)
            .with("m", 1i64)
            .with(KEY_A, 1i64);
        let keys: Vec<&str> = set.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![KEY_A, "m", "z"]);
    }

    #[test]
    fn with_opt_skips_none() {
        let set = AttrSet::new()
            .with_opt(KEY_A, Some(1i64))
            .with_opt::<i64>(KEY_B, None);
        assert_eq!(count(&set), 1);
    }
}
