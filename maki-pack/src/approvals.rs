//! Which permissions the user approved, for which package, from which source.
//!
//! Approvals live outside every checkout. A downloaded `plugin.toml` states
//! what a package *wants*; this file states what it may have. The manifest is
//! never authority over itself.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// An approval is bound to the source as well as the name.
///
/// Without the source in the key, deleting a package and adding an unrelated
/// repository under the same name would silently inherit the old grants. A
/// changed `src` is a new trust decision, so it must not match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalKey {
    pub name: String,
    pub src: String,
}

impl ApprovalKey {
    pub fn new(name: impl Into<String>, src: &str) -> Self {
        Self {
            name: name.into(),
            src: normalize_src(src),
        }
    }
}

/// Canonical form of a source, for keying an approval.
///
/// This deliberately does almost nothing. An earlier version folded a `.git`
/// suffix, a trailing slash, and scheme and host case, on the theory that those
/// name the same repository. They do not always: `/x/repo` and `/x/repo.git`
/// can both exist as separate local repositories, and lowercasing an authority
/// also lowercases any user information in an ssh URL. Every one of those
/// collisions hands a different repository someone else's permission grants.
///
/// Being too strict only costs a second prompt when a user writes the same URL
/// two ways. Being too loose grants access that was never approved, so this
/// errs strict and trims whitespace alone.
fn normalize_src(src: &str) -> String {
    src.trim().to_owned()
}

/// The approval store, as written to disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Approvals {
    #[serde(default)]
    entries: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    src: String,
    #[serde(default)]
    permissions: Vec<String>,
}

impl Approvals {
    /// Permissions approved for this exact package and source. A package whose
    /// source changed has no approval, which is the point.
    pub fn get(&self, key: &ApprovalKey) -> Option<&[String]> {
        let entry = self.entries.get(&key.name)?;
        (entry.src == key.src).then_some(entry.permissions.as_slice())
    }

    pub fn approve(&mut self, key: &ApprovalKey, permissions: Vec<String>) {
        self.entries.insert(
            key.name.clone(),
            Entry {
                src: key.src.clone(),
                permissions,
            },
        );
    }

    /// Drops a package's approval, so reinstalling it asks again.
    pub fn revoke(&mut self, name: &str) -> bool {
        self.entries.remove(name).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const SRC: &str = "https://github.com/user/repo";

    fn approved(store: &Approvals, name: &str, src: &str) -> Option<Vec<String>> {
        store
            .get(&ApprovalKey::new(name, src))
            .map(<[String]>::to_vec)
    }

    /// The defect this key shape exists to prevent: reusing a name must not
    /// inherit the previous repository's grants.
    #[test]
    fn approval_does_not_carry_over_to_a_different_source() {
        let mut store = Approvals::default();
        store.approve(&ApprovalKey::new("pkg", SRC), vec!["run".to_owned()]);
        assert_eq!(
            approved(&store, "pkg", "https://github.com/attacker/repo"),
            None,
            "a different repository must not inherit the approval"
        );
    }

    /// Only surrounding whitespace is ignored. Anything else is a different
    /// source and must be approved again.
    #[test]
    fn whitespace_around_the_same_source_keeps_the_approval() {
        let mut store = Approvals::default();
        store.approve(&ApprovalKey::new("pkg", SRC), vec!["net".to_owned()]);
        let padded = format!("  {SRC}\n");
        assert!(approved(&store, "pkg", &padded).is_some());
    }

    /// These all look like harmless spellings, and an earlier version folded
    /// them together. Each can name a genuinely different repository, and
    /// folding them lets one inherit another's grants.
    #[test_case("https://github.com/user/repo.git" ; "git_suffix_may_be_a_separate_repo")]
    #[test_case("https://github.com/user/repo/" ; "trailing_slash")]
    #[test_case("HTTPS://GitHub.com/user/repo" ; "authority_case")]
    #[test_case("https://github.com/USER/repo" ; "path_case")]
    #[test_case("ssh://User@github.com/user/repo" ; "ssh_user_case")]
    fn a_differently_spelled_source_is_not_approved(variant: &str) {
        let mut store = Approvals::default();
        store.approve(&ApprovalKey::new("pkg", SRC), vec!["net".to_owned()]);
        assert!(
            approved(&store, "pkg", variant).is_none(),
            "{variant} must be a fresh trust decision"
        );
    }

    /// Two local repositories that really can both exist side by side.
    #[test]
    fn a_local_path_and_its_git_suffix_are_distinct() {
        let mut store = Approvals::default();
        store.approve(
            &ApprovalKey::new("pkg", "/srv/repo"),
            vec!["run".to_owned()],
        );
        assert!(approved(&store, "pkg", "/srv/repo.git").is_none());
    }

    #[test]
    fn round_trips_through_json() {
        let mut store = Approvals::default();
        store.approve(&ApprovalKey::new("pkg", SRC), vec!["net".to_owned()]);

        let text = serde_json::to_string(&store).unwrap();
        let back: Approvals = serde_json::from_str(&text).unwrap();
        assert_eq!(approved(&back, "pkg", SRC), Some(vec!["net".to_owned()]));
    }
}
