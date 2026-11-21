use std::collections::BTreeSet;
use std::fs;
use std::iter;

use serde::{Deserialize, Serialize};

use crate::sessions::StoredThinking;
use crate::{StateDir, atomic_write};

const MODEL_FILE: &str = "model";
const RECENT_FILE: &str = "recent-models";
const THINKING_FILE: &str = "thinking";
const EXPANDED_FILE: &str = "expanded-providers";
const MAX_RECENTS: usize = 6;

pub fn persist_model(dir: &StateDir, spec: &str) {
    let _ = atomic_write(&dir.path().join(MODEL_FILE), spec.as_bytes());
}

pub fn read_model(dir: &StateDir) -> Option<String> {
    let raw = fs::read_to_string(dir.path().join(MODEL_FILE)).ok()?;
    let spec = raw.trim();
    (!spec.is_empty()).then(|| spec.to_owned())
}

/// Written in the `/thinking` spelling so the file reads like the command
/// that produced it, and a hand-edited or stale value just fails to parse.
pub fn persist_thinking(dir: &StateDir, thinking: StoredThinking) {
    let _ = atomic_write(
        &dir.path().join(THINKING_FILE),
        thinking.to_string().as_bytes(),
    );
}

pub fn read_thinking(dir: &StateDir) -> Option<StoredThinking> {
    let raw = fs::read_to_string(dir.path().join(THINKING_FILE)).ok()?;
    StoredThinking::parse_setting(&raw).ok()
}

/// Providers the model picker lists unfolded, by slug, since it folds the rest.
/// Kept as a sorted set, so unfolding and folding the same provider leaves the
/// file as it was.
pub fn read_expanded_providers(dir: &StateDir) -> BTreeSet<String> {
    fs::read_to_string(dir.path().join(EXPANDED_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn persist_expanded_providers(dir: &StateDir, slugs: &BTreeSet<String>) {
    if let Ok(json) = serde_json::to_vec_pretty(slugs) {
        let _ = atomic_write(&dir.path().join(EXPANDED_FILE), &json);
    }
}

#[derive(Serialize, Deserialize, Default)]
struct RecentList(Vec<String>);

/// Merges onto the list on disk, so picks made by another maki since this one
/// read it survive.
pub fn push_recent(dir: &StateDir, spec: &str) {
    write_recents(dir, &with_recent(&read_recents(dir), spec));
}

pub fn with_recent(recents: &[String], spec: &str) -> Vec<String> {
    iter::once(spec)
        .chain(recents.iter().map(String::as_str).filter(|s| *s != spec))
        .take(MAX_RECENTS)
        .map(str::to_owned)
        .collect()
}

/// Newest first, so a list written by a launch that predates the migration
/// does not bury the history it replaced. Identical to `with_recent` for one
/// spec, applied to a whole list.
pub fn merge_recents(new: &[String], old: &[String]) -> Vec<String> {
    let mut merged = new.to_vec();
    for spec in old {
        if !merged.contains(spec) {
            merged.push(spec.clone());
        }
    }
    merged.truncate(MAX_RECENTS);
    merged
}

pub fn read_recents(dir: &StateDir) -> Vec<String> {
    let Ok(raw) = fs::read_to_string(dir.path().join(RECENT_FILE)) else {
        return Vec::new();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<RecentList>(trimmed) {
        Ok(list) => list.0,
        Err(_) => Vec::new(),
    }
}

fn write_recents(dir: &StateDir, recents: &[String]) {
    let json = match serde_json::to_vec_pretty(&RecentList(recents.to_vec())) {
        Ok(v) => v,
        Err(_) => return,
    };
    let _ = atomic_write(&dir.path().join(RECENT_FILE), &json);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::Effort;
    use tempfile::TempDir;
    use test_case::test_case;

    const OLD_SPEC: &str = "anthropic/claude-sonnet-4";
    const NEW_SPEC: &str = "openai/gpt-5.4-nano";

    #[test]
    fn round_trip() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        assert!(read_model(&dir).is_none());

        persist_model(&dir, "anthropic/claude-sonnet-4");
        assert_eq!(
            read_model(&dir).as_deref(),
            Some("anthropic/claude-sonnet-4")
        );

        persist_model(&dir, "openai/gpt-5.4-nano");
        assert_eq!(read_model(&dir).as_deref(), Some("openai/gpt-5.4-nano"));
    }

    #[test]
    fn whitespace_and_empty_treated_as_none() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        fs::write(dir.path().join(MODEL_FILE), "  \n").unwrap();
        assert!(read_model(&dir).is_none());

        fs::write(dir.path().join(MODEL_FILE), "").unwrap();
        assert!(read_model(&dir).is_none());
    }

    #[test_case(StoredThinking::Off)]
    #[test_case(StoredThinking::Adaptive)]
    #[test_case(StoredThinking::Effort { level: Effort::High })]
    #[test_case(StoredThinking::Budget { tokens: 8192 })]
    fn thinking_round_trip(thinking: StoredThinking) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        assert!(read_thinking(&dir).is_none());
        persist_thinking(&dir, thinking);
        assert_eq!(read_thinking(&dir), Some(thinking));
    }

    #[test_case("" ; "empty")]
    #[test_case("  \n" ; "whitespace")]
    #[test_case("turbo" ; "unknown level")]
    #[test_case("0" ; "zero budget")]
    fn read_thinking_ignores_invalid(raw: &str) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        fs::write(dir.path().join(THINKING_FILE), raw).unwrap();
        assert!(read_thinking(&dir).is_none());
    }

    #[test]
    fn push_recent_merges_onto_the_list_on_disk() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        push_recent(&dir, OLD_SPEC);
        push_recent(&dir, NEW_SPEC);
        assert_eq!(read_recents(&dir), [NEW_SPEC, OLD_SPEC]);
    }

    #[test_case(&[], NEW_SPEC, &[NEW_SPEC] ; "first pick")]
    #[test_case(&[OLD_SPEC], NEW_SPEC, &[NEW_SPEC, OLD_SPEC] ; "newest first")]
    #[test_case(&[NEW_SPEC, OLD_SPEC], OLD_SPEC, &[OLD_SPEC, NEW_SPEC] ; "repeat pick moves up")]
    fn with_recent_puts_the_pick_first(recents: &[&str], spec: &str, expected: &[&str]) {
        let recents: Vec<String> = recents.iter().map(|s| s.to_string()).collect();
        assert_eq!(with_recent(&recents, spec), expected);
    }

    #[test]
    fn with_recent_drops_the_oldest_past_the_cap() {
        let full: Vec<String> = (0..MAX_RECENTS).map(|i| format!("p/model-{i}")).collect();
        let recents = with_recent(&full, NEW_SPEC);
        assert_eq!(recents[0], NEW_SPEC);
        assert_eq!(recents[1..], full[..MAX_RECENTS - 1]);
    }

    #[test]
    fn merge_recents_keeps_both_sides_newest_first() {
        let merged = merge_recents(
            &["new/one".to_owned()],
            &["old/one".to_owned(), "new/one".to_owned()],
        );
        assert_eq!(merged, ["new/one", "old/one"]);
    }

    #[test]
    fn merge_recents_caps_at_max() {
        let new: Vec<String> = (0..MAX_RECENTS).map(|i| format!("new/{i}")).collect();
        let old: Vec<String> = (0..MAX_RECENTS).map(|i| format!("old/{i}")).collect();
        assert_eq!(merge_recents(&new, &old).len(), MAX_RECENTS);
    }

    #[test]
    fn read_recents_handles_missing_or_invalid() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        assert!(read_recents(&dir).is_empty());

        fs::write(dir.path().join(RECENT_FILE), "not json").unwrap();
        assert!(read_recents(&dir).is_empty());

        fs::write(dir.path().join(RECENT_FILE), "  \n").unwrap();
        assert!(read_recents(&dir).is_empty());
    }
}
