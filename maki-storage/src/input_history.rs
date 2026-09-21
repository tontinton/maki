use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths::project_dir;
use crate::{StateDir, StorageError, atomic_write};

const HISTORY_FILE: &str = "input_history.json";
pub const MAX_ENTRIES: usize = 100;

fn history_path(dir: &StateDir, cwd: &Path) -> PathBuf {
    project_dir(dir, cwd).join(HISTORY_FILE)
}

#[derive(Debug)]
pub struct InputHistory {
    entries: VecDeque<String>,
    max_entries: usize,
    /// Where [`Self::save`] writes, fixed at load time. `/cd` loads a new
    /// history rather than re-deriving this, so the prompts typed here can
    /// never overwrite another project's file.
    file: Option<PathBuf>,
}

impl Default for InputHistory {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            max_entries: MAX_ENTRIES,
            file: None,
        }
    }
}

impl InputHistory {
    pub fn load(dir: &StateDir, cwd: &Path, max_entries: usize) -> Self {
        let file = history_path(dir, cwd);
        let mut history = Self {
            entries: VecDeque::with_capacity(max_entries),
            max_entries,
            file: Some(file),
        };
        let Some(data) = history.file.as_ref().and_then(|p| fs::read(p).ok()) else {
            return history;
        };
        let items: Vec<String> = serde_json::from_slice(&data).unwrap_or_default();
        for entry in items {
            history.push_inner(entry);
        }
        history
    }

    /// A history with no file behind it (the [`Default`] one tests build) has
    /// nowhere to go, so saving it is a no-op rather than an error.
    pub fn save(&self) -> Result<(), StorageError> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(file, &serde_json::to_vec(&self.entries)?)
    }

    pub fn push(&mut self, entry: String) {
        let trimmed = entry.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        self.push_inner(trimmed);
    }

    fn push_inner(&mut self, entry: String) {
        if self.entries.back().is_some_and(|last| *last == entry) {
            return;
        }
        if self.entries.len() == self.max_entries {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(String::as_str)
    }

    pub fn max_entries(&self) -> usize {
        self.max_entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::GIT_MARKER;
    use tempfile::TempDir;
    use test_case::test_case;

    const STATE_SUBDIR: &str = "state";
    const REPO_DIR: &str = "repo";
    const SUBDIR: &str = "src";

    fn tmp_dir() -> (TempDir, StateDir) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(tmp.path().join(STATE_SUBDIR));
        (tmp, dir)
    }

    fn project(tmp: &TempDir, name: &str) -> PathBuf {
        let path = tmp.path().join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn roundtrip() {
        let (tmp, dir) = tmp_dir();
        let cwd = project(&tmp, "one");
        let mut history = InputHistory::load(&dir, &cwd, MAX_ENTRIES);
        history.push("a".into());
        history.push("b".into());
        history.push("c".into());
        history.save().unwrap();
        let loaded = InputHistory::load(&dir, &cwd, MAX_ENTRIES);
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.get(0), Some("a"));
        assert_eq!(loaded.get(2), Some("c"));
    }

    #[test]
    fn each_project_keeps_its_own_entries() {
        let (tmp, dir) = tmp_dir();
        let (one, two) = (project(&tmp, "one"), project(&tmp, "two"));

        let mut first = InputHistory::load(&dir, &one, MAX_ENTRIES);
        first.push("in one".into());
        first.save().unwrap();

        let mut second = InputHistory::load(&dir, &two, MAX_ENTRIES);
        assert!(second.is_empty());
        second.push("in two".into());
        second.save().unwrap();

        assert_eq!(
            InputHistory::load(&dir, &one, MAX_ENTRIES).get(0),
            Some("in one")
        );
        assert_eq!(
            InputHistory::load(&dir, &two, MAX_ENTRIES).get(0),
            Some("in two")
        );
    }

    #[test]
    fn a_repo_subdir_shares_the_repo_history() {
        let (tmp, dir) = tmp_dir();
        let repo = project(&tmp, REPO_DIR);
        fs::create_dir(repo.join(GIT_MARKER)).unwrap();
        let subdir = project(&tmp, &format!("{REPO_DIR}/{SUBDIR}"));

        let mut history = InputHistory::load(&dir, &repo, MAX_ENTRIES);
        history.push("a".into());
        history.save().unwrap();

        assert_eq!(history_path(&dir, &subdir), history_path(&dir, &repo));
        assert_eq!(
            InputHistory::load(&dir, &subdir, MAX_ENTRIES).get(0),
            Some("a")
        );
    }

    #[test]
    fn a_saved_project_history_stays_capped() {
        const MAX: usize = 3;

        let (tmp, dir) = tmp_dir();
        let cwd = project(&tmp, "one");
        let mut history = InputHistory::load(&dir, &cwd, MAX);
        for i in 0..MAX * 2 {
            history.push(format!("entry{i}"));
        }
        history.save().unwrap();

        let loaded = InputHistory::load(&dir, &cwd, MAX);
        assert_eq!(loaded.len(), MAX);
        assert_eq!(loaded.get(0), Some("entry3"));
    }

    #[test]
    fn a_history_with_no_file_saves_nowhere() {
        let history = InputHistory::default();
        history.save().unwrap();
    }

    #[test]
    fn truncates_to_max_entries() {
        let mut history = InputHistory::default();
        for i in 0..150 {
            history.push(format!("entry{i}"));
        }
        assert_eq!(history.len(), MAX_ENTRIES);
        assert_eq!(history.get(0), Some("entry50"));
        assert_eq!(history.get(MAX_ENTRIES - 1), Some("entry149"));
    }

    #[test]
    fn rejects_consecutive_duplicates() {
        let mut history = InputHistory::default();
        history.push("a".into());
        history.push("a".into());
        history.push("b".into());
        history.push("b".into());
        history.push("a".into());
        assert_eq!(history.len(), 3);
        assert_eq!(history.get(0), Some("a"));
        assert_eq!(history.get(1), Some("b"));
        assert_eq!(history.get(2), Some("a"));
    }

    #[test]
    fn push_trims_and_rejects_blank() {
        let mut history = InputHistory::default();
        history.push("".into());
        history.push("   ".into());
        history.push("\n".into());
        assert!(history.is_empty());

        history.push("  hello  ".into());
        assert_eq!(history.get(0), Some("hello"));
    }

    #[test_case(None      ; "missing_file")]
    #[test_case(Some(b"not json" as &[u8]) ; "corrupt_file")]
    fn load_bad_state_returns_empty(content: Option<&[u8]>) {
        let (tmp, dir) = tmp_dir();
        let cwd = project(&tmp, "one");
        if let Some(data) = content {
            let path = history_path(&dir, &cwd);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, data).unwrap();
        }
        let history = InputHistory::load(&dir, &cwd, MAX_ENTRIES);
        assert!(history.is_empty());
    }
}
