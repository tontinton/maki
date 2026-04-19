use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use crate::ToolOutput;
use futures_lite::StreamExt;
use maki_tool_macro::Tool;
use serde::Deserialize;

const MAX_LINES_PER_FILE: usize = 200;
const MAX_DIR_BYTES: u64 = 50 * 1024;
const VALID_COMMANDS: &[&str] = &["view", "write", "delete"];

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Memory {
    #[param(description = "Command: view, write, delete")]
    command: String,
    #[param(description = "Relative path (e.g. 'architecture.md'). Omit to list all.")]
    path: Option<String>,
    #[param(description = "File content for 'write'")]
    content: Option<String>,
}

impl Memory {
    pub const NAME: &str = "memory";
    pub const DESCRIPTION: &str = include_str!("memory.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[{"command": "write", "path": "gotchas.md", "content": "- DB migrations require `just migrate` before running tests"}]"#,
    );

    pub async fn execute(&self, _ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let memories_dir = resolve_memories_dir()?;
        dispatch(
            &self.command,
            self.path.as_deref(),
            self.content.as_deref(),
            &memories_dir,
        )
        .await
    }

    pub fn start_summary(&self) -> String {
        match self.path.as_deref() {
            Some(p) => format!("{} {p}", self.command),
            None => self.command.clone(),
        }
    }
}

super::impl_tool!(
    Memory,
    audience = super::ToolAudience::MAIN | super::ToolAudience::GENERAL_SUB,
);

impl super::ToolInvocation for Memory {
    fn start_summary(&self) -> super::SummaryFuture {
        super::SummaryFuture::Ready(Memory::start_summary(self))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Memory::execute(&self, ctx).await })
    }
}

async fn dispatch(
    command: &str,
    path: Option<&str>,
    content: Option<&str>,
    memories_dir: &Path,
) -> Result<ToolOutput, String> {
    match command {
        "view" => cmd_view(path, memories_dir).await,
        "write" => {
            cmd_write(
                path.ok_or("'path' is required for write")?,
                content.ok_or("'content' is required for write")?,
                memories_dir,
            )
            .await
        }
        "delete" => cmd_delete(path.ok_or("'path' is required for delete")?, memories_dir)
            .await
            .map(ToolOutput::Plain),
        _ => Err(format!(
            "unknown command '{command}'. Valid commands: {}",
            VALID_COMMANDS.join(", ")
        )),
    }
}

async fn cmd_view(path: Option<&str>, memories_dir: &Path) -> Result<ToolOutput, String> {
    match path {
        None => list_memories(memories_dir).await.map(ToolOutput::Plain),
        Some(p) => {
            let file_path = safe_resolve(memories_dir, p)?;
            let content = smol::fs::read_to_string(&file_path)
                .await
                .map_err(|e| format!("read error: {e}"))?;
            Ok(ToolOutput::MemoryRead {
                path: p.to_owned(),
                lines: content.lines().map(ToOwned::to_owned).collect(),
            })
        }
    }
}

async fn cmd_write(path: &str, content: &str, memories_dir: &Path) -> Result<ToolOutput, String> {
    let line_count = content.lines().count().max(1);
    if line_count > MAX_LINES_PER_FILE {
        return Err(format!(
            "content exceeds {MAX_LINES_PER_FILE} lines ({line_count} lines); reduce content size"
        ));
    }

    let file_path = safe_resolve(memories_dir, path)?;
    let existing_size = smol::fs::metadata(&file_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let new_size = content.len() as u64;
    let dir_size = dir_total_bytes(memories_dir).await;
    if dir_size - existing_size + new_size > MAX_DIR_BYTES {
        return Err(format!(
            "memory directory would exceed {MAX_DIR_BYTES} byte limit; delete stale entries first"
        ));
    }

    smol::fs::create_dir_all(memories_dir)
        .await
        .map_err(|e| format!("mkdir error: {e}"))?;
    smol::fs::write(&file_path, content)
        .await
        .map_err(|e| format!("write error: {e}"))?;
    Ok(ToolOutput::MemoryWrite {
        path: path.to_owned(),
        lines: content.lines().map(ToOwned::to_owned).collect(),
    })
}

async fn cmd_delete(path: &str, memories_dir: &Path) -> Result<String, String> {
    let file_path = safe_resolve(memories_dir, path)?;
    if smol::fs::metadata(&file_path).await.is_err() {
        return Err(format!("'{path}' does not exist"));
    }
    smol::fs::remove_file(&file_path)
        .await
        .map_err(|e| format!("delete error: {e}"))?;
    Ok(format!("deleted {path}"))
}

async fn list_memories(memories_dir: &Path) -> Result<String, String> {
    let mut dir = match smol::fs::read_dir(memories_dir).await {
        Ok(d) => d,
        Err(_) => return Ok("No memories yet.".into()),
    };
    let mut entries: Vec<(String, u64)> = Vec::new();
    while let Some(entry) = dir.next().await {
        let entry = entry.map_err(|e| format!("read dir error: {e}"))?;
        let meta = entry
            .metadata()
            .await
            .map_err(|e| format!("metadata error: {e}"))?;
        if meta.is_file()
            && let Some(name) = entry.file_name().to_str()
        {
            entries.push((name.to_string(), meta.len()));
        }
    }
    if entries.is_empty() {
        return Ok("No memories yet.".into());
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::new();
    for (name, size) in &entries {
        let _ = writeln!(out, "{name} ({size} bytes)");
    }
    let total: u64 = entries.iter().map(|(_, s)| *s).sum();
    let _ = write!(out, "\n{} files, {total} bytes total", entries.len());
    Ok(out)
}

fn safe_resolve(memories_dir: &Path, relative: &str) -> Result<PathBuf, String> {
    let rel = Path::new(relative);
    if rel.is_absolute() {
        return Err("path must be relative".into());
    }
    let joined = memories_dir.join(rel);
    let canonical_base = memories_dir
        .canonicalize()
        .or_else(|_| fs::create_dir_all(memories_dir).and_then(|_| memories_dir.canonicalize()))
        .map_err(|e| format!("resolve error: {e}"))?;

    let canonical = if joined.exists() {
        joined
            .canonicalize()
            .map_err(|e| format!("resolve error: {e}"))?
    } else {
        let parent = joined.parent().ok_or("invalid path")?;
        let file_name = joined.file_name().ok_or("invalid path")?;
        let canonical_parent = if parent.exists() {
            parent
                .canonicalize()
                .map_err(|e| format!("resolve error: {e}"))?
        } else {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir error: {e}"))?;
            parent
                .canonicalize()
                .map_err(|e| format!("resolve error: {e}"))?
        };
        canonical_parent.join(file_name)
    };

    if !canonical.starts_with(&canonical_base) {
        return Err("path traversal outside memories directory is not allowed".into());
    }
    Ok(canonical)
}

async fn dir_total_bytes(dir: &Path) -> u64 {
    let Ok(mut entries) = smol::fs::read_dir(dir).await else {
        return 0;
    };
    let mut total = 0;
    while let Some(Ok(entry)) = entries.next().await {
        if let Ok(meta) = entry.metadata().await
            && meta.is_file()
        {
            total += meta.len();
        }
    }
    total
}

pub fn resolve_memories_dir() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot get cwd: {e}"))?;
    let root = find_git_root(&cwd);
    let project_id = project_id(&root);
    let home = dirs::home_dir().ok_or("home directory not found")?;
    Ok(home
        .join(".maki")
        .join("projects")
        .join(project_id)
        .join("memories"))
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
    let memories_dir = resolve_memories_dir().ok()?;
    if !memories_dir.exists() {
        return None;
    }
    let entries: Vec<(String, u64)> = fs::read_dir(&memories_dir)
        .ok()?
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
    if entries.is_empty() {
        return None;
    }
    let mut sorted = entries;
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    Some(sorted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        smol::block_on(f)
    }

    fn tmp_memories() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let memories = dir.path().join("memories");
        (dir, memories)
    }

    #[test]
    fn project_id_includes_basename() {
        let id = project_id(Path::new("/home/user/my-project"));
        assert!(id.starts_with("my-project-"));
    }

    #[test]
    fn find_git_root_finds_ancestor_with_git() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a").join("b").join("c");
        fs::create_dir_all(&sub).unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        assert_eq!(find_git_root(&sub), dir.path());
    }

    #[test]
    fn find_git_root_falls_back_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("no_git");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_git_root(&sub), sub);
    }

    #[test_case("../escape"        ; "dotdot_traversal")]
    #[test_case("/etc/passwd"       ; "absolute_path")]
    fn safe_resolve_rejects_traversal(path: &str) {
        let dir = tempfile::tempdir().unwrap();
        let result = safe_resolve(dir.path(), path);
        assert!(result.is_err(), "should reject: {path}");
    }

    #[test]
    fn write_view_overwrite_list_delete_lifecycle() {
        let (_dir, memories) = tmp_memories();

        let list = run(cmd_view(None, &memories)).unwrap().as_display_text();
        assert_eq!(list, "No memories yet.");

        let content = "# Architecture\nMicroservices";
        run(cmd_write("arch.md", content, &memories)).unwrap();
        let viewed = run(cmd_view(Some("arch.md"), &memories))
            .unwrap()
            .as_display_text();
        assert_eq!(viewed, content);

        run(cmd_write("arch.md", "v2", &memories)).unwrap();
        let viewed = run(cmd_view(Some("arch.md"), &memories))
            .unwrap()
            .as_display_text();
        assert_eq!(viewed, "v2");

        run(cmd_write("notes.md", "hello", &memories)).unwrap();
        let listing = run(cmd_view(None, &memories)).unwrap().as_display_text();
        assert!(listing.contains("arch.md"));
        assert!(listing.contains("notes.md"));
        assert!(listing.contains("2 files"));

        run(cmd_delete("arch.md", &memories)).unwrap();
        assert!(run(cmd_view(Some("arch.md"), &memories)).is_err());
    }

    #[test]
    fn delete_nonexistent_errors() {
        let (_dir, memories) = tmp_memories();
        fs::create_dir_all(&memories).unwrap();
        assert!(run(cmd_delete("nope.md", &memories)).is_err());
    }

    #[test]
    fn write_rejects_too_many_lines() {
        let (_dir, memories) = tmp_memories();
        let content = "line\n".repeat(MAX_LINES_PER_FILE + 1);
        let err = run(cmd_write("big.md", &content, &memories)).unwrap_err();
        assert!(err.contains(&MAX_LINES_PER_FILE.to_string()));
    }

    #[test]
    fn dispatch_rejects_invalid_command() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dispatch("veiw", None, None, dir.path())).unwrap_err();
        assert!(err.contains("unknown command"));
    }
}
