use std::fs;

use serde::{Deserialize, Serialize};

use crate::sessions::StoredThinking;
use crate::{StateDir, atomic_write};

const MODEL_FILE: &str = "model";
const RECENT_FILE: &str = "recent-models";
const THINKING_FILE: &str = "thinking";
const MAX_RECENTS: usize = 4;

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

#[derive(Serialize, Deserialize, Default)]
struct RecentList(Vec<String>);

pub fn push_recent(dir: &StateDir, spec: &str) -> Vec<String> {
    let mut recents = read_recents(dir);
    recents.retain(|s| s != spec);
    recents.insert(0, spec.to_owned());
    recents.truncate(MAX_RECENTS);
    write_recents(dir, &recents);
    recents
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
    fn push_recent_dedupes_and_orders_most_recent_first() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        push_recent(&dir, "anthropic/claude-sonnet-4");
        push_recent(&dir, "openai/gpt-5.4-nano");
        assert_eq!(
            read_recents(&dir),
            ["openai/gpt-5.4-nano", "anthropic/claude-sonnet-4"]
        );

        push_recent(&dir, "anthropic/claude-sonnet-4");
        assert_eq!(
            read_recents(&dir),
            ["anthropic/claude-sonnet-4", "openai/gpt-5.4-nano"]
        );
    }

    #[test]
    fn push_recent_caps_at_max() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        for i in 0..(MAX_RECENTS + 3) {
            push_recent(&dir, &format!("p/model-{i}"));
        }
        let recents = read_recents(&dir);
        assert_eq!(recents.len(), MAX_RECENTS);
        assert_eq!(recents[0], format!("p/model-{}", MAX_RECENTS + 2));
    }

    #[test]
    fn push_recent_returns_final_list() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        let recents = push_recent(&dir, "a/b");
        assert_eq!(recents, vec!["a/b".to_string()]);
        assert_eq!(recents, read_recents(&dir));
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
