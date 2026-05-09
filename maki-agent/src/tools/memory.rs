use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

fn memories_path_suffix() -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot get cwd: {e}"))?;
    let root = find_git_root(&cwd);
    let id = project_id(&root);
    Ok(format!("projects/{id}/memories"))
}

fn resolve_memories_dir() -> Result<PathBuf, String> {
    let suffix = memories_path_suffix()?;
    let state = maki_storage::paths::state_dir().map_err(|e| format!("state dir: {e}"))?;
    Ok(state.join(suffix))
}

fn resolve_memories_read_dir() -> Result<PathBuf, String> {
    if let Some(legacy) = maki_storage::paths::legacy_home_dir() {
        let dir = legacy.join(memories_path_suffix()?);
        if dir.is_dir() {
            return Ok(dir);
        }
    }
    resolve_memories_dir()
}

fn find_git_root(start: &Path) -> PathBuf {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return current;
        }
        if !current.pop() {
            return start.to_path_buf();
        }
    }
}

fn project_id(path: &Path) -> String {
    let basename = path.file_name().and_then(|n| n.to_str()).unwrap_or("root");
    let hash = fnv1a_64(path.to_string_lossy().as_bytes());
    format!("{basename}-{hash:016x}")
}

fn fnv1a_64(data: &[u8]) -> u64 {
    const BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = BASIS;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

pub fn list_memory_files() -> Option<String> {
    let sorted = list_memory_entries()?;
    let mut out = String::from("\n\nMemory files (use the memory tool to view/update):\n");
    for (name, size) in &sorted {
        let _ = writeln!(out, "- {name} ({size} bytes)");
    }
    Some(out)
}

pub fn list_memory_entries() -> Option<Vec<(String, u64)>> {
    let memories_dir = resolve_memories_read_dir().ok()?;
    let entries = collect_file_entries(&memories_dir);
    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
}

fn collect_file_entries(dir: &Path) -> Vec<(String, u64)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<(String, u64)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if meta.is_file() {
                Some((e.file_name().to_string_lossy().into_owned(), meta.len()))
            } else {
                None
            }
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn fnv1a_64_pinned() {
        assert_eq!(fnv1a_64(b"/home/user/my-project"), 0xfc6e8b528feefa1c);
    }

    #[test]
    fn fnv1a_64_empty_is_basis() {
        assert_eq!(fnv1a_64(b""), 0xcbf29ce484222325);
    }

    #[test]
    fn fnv1a_64_single_byte() {
        assert_eq!(fnv1a_64(b"a"), 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn fnv1a_64_order_matters() {
        assert_ne!(fnv1a_64(b"ab"), fnv1a_64(b"ba"));
    }

    #[test]
    fn project_id_includes_basename() {
        let id = project_id(Path::new("/home/user/my-project"));
        assert!(id.starts_with("my-project-"));
    }

    #[test]
    fn project_id_root_path_uses_root() {
        let id = project_id(Path::new("/"));
        assert!(
            id.starts_with("root-"),
            "/ should produce root- prefix, got: {id}"
        );
    }

    #[test]
    fn project_id_different_paths_same_basename_differ() {
        let a = project_id(Path::new("/home/alice/app"));
        let b = project_id(Path::new("/home/bob/app"));
        assert_ne!(a, b, "full path must factor into ID");
    }

    #[test_case("/home/user/repo", "/home/user/repo" ; "at_git_root")]
    #[test_case("/home/user/repo/src/lib", "/home/user/repo" ; "deep_subdir")]
    fn find_git_root_with_git_dir(start: &str, expected: &str) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("home/user/repo");
        let src = repo.join("src/lib");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();

        let actual_start = dir.path().join(start.trim_start_matches('/'));
        let actual_expected = dir.path().join(expected.trim_start_matches('/'));
        assert_eq!(find_git_root(&actual_start), actual_expected);
    }

    #[test]
    fn find_git_root_no_git_returns_start() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("no_git_here");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_git_root(&sub), sub);
    }

    #[test]
    fn collect_entries_returns_sorted_files_with_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let memories = dir.path().join("memories");
        fs::create_dir_all(&memories).unwrap();
        fs::write(memories.join("arch.md"), "data").unwrap();
        fs::write(memories.join("notes.md"), "more").unwrap();

        let entries = collect_file_entries(&memories);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "arch.md");
        assert_eq!(entries[0].1, 4);
        assert_eq!(entries[1].0, "notes.md");
        assert_eq!(entries[1].1, 4);
    }

    #[test]
    fn collect_entries_empty_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let memories = dir.path().join("memories");
        fs::create_dir_all(&memories).unwrap();
        assert!(collect_file_entries(&memories).is_empty());
    }

    #[test]
    fn collect_entries_missing_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let memories = dir.path().join("nonexistent");
        assert!(collect_file_entries(&memories).is_empty());
    }
}
