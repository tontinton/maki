//! The exact commit of every managed package.
//!
//! Committing this file and running an install reproduces a package set on
//! another machine, which is the whole reason it records commits rather than
//! the `version` that produced them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One recorded package. The source is stored alongside the revision so a
/// lockfile entry describes where its commit came from, not just what it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockEntry {
    pub src: String,
    pub rev: String,
}

/// A `BTreeMap` rather than a hash map, so the file serializes in a stable
/// order and committing it does not produce noisy diffs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    #[serde(default)]
    packages: BTreeMap<String, LockEntry>,
}

impl Lockfile {
    pub fn get(&self, name: &str) -> Option<&LockEntry> {
        self.packages.get(name)
    }

    /// Records a package at a revision. Called only after the change on disk
    /// succeeded, so a failed fetch never moves a recorded revision.
    pub fn record(
        &mut self,
        name: impl Into<String>,
        src: impl Into<String>,
        rev: impl Into<String>,
    ) {
        self.packages.insert(
            name.into(),
            LockEntry {
                src: src.into(),
                rev: rev.into(),
            },
        );
    }

    /// Every recorded name, alphabetically. Pruning and `pack.get` walk the
    /// lockfile this way, so neither depends on insertion order.
    pub fn install_order(&self) -> impl Iterator<Item = &str> {
        self.packages.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled() -> Lockfile {
        let mut lock = Lockfile::default();
        lock.record("zebra", "https://x/zebra", "ccc");
        lock.record("alpha", "https://x/alpha", "aaa");
        lock.record("mid", "https://x/mid", "bbb");
        lock
    }

    /// Alphabetical, so what a walk of the lockfile reports is the same on
    /// every machine that shares it.
    #[test]
    fn install_order_is_alphabetical_not_insertion_order() {
        assert_eq!(
            filled().install_order().collect::<Vec<_>>(),
            ["alpha", "mid", "zebra"]
        );
    }

    #[test]
    fn round_trips_through_json() {
        let lock = filled();
        let back = Lockfile::from_json(&lock.to_json().unwrap()).unwrap();
        assert_eq!(back, lock);
    }

    /// The file is meant to be committed, so the same content must serialize
    /// identically however it was built.
    #[test]
    fn serialization_is_stable_regardless_of_insertion_order() {
        let mut other = Lockfile::default();
        other.record("alpha", "https://x/alpha", "aaa");
        other.record("mid", "https://x/mid", "bbb");
        other.record("zebra", "https://x/zebra", "ccc");
        assert_eq!(other.to_json().unwrap(), filled().to_json().unwrap());
    }

    #[test]
    fn missing_packages_key_reads_as_empty() {
        assert_eq!(
            Lockfile::from_json("{}").unwrap().install_order().count(),
            0
        );
    }
}
