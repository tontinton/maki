use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use mlua::{Buffer, Lua, Result as LuaResult, Table};

const SANDBOX_ERR: &str = "path outside sandbox";

/// Resolve `path` for sandbox checking: canonicalize the deepest ancestor that exists
/// (catching symlink escapes), then re-append remaining components lexically.
/// This lets plugins write to paths that don't exist yet while still blocking
/// symlink escapes through any component that does exist.
fn resolve_for_sandbox(path: &str) -> LuaResult<PathBuf> {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| mlua::Error::runtime(format!("fs: cannot resolve cwd: {e}")))?
            .join(p)
    };

    let mut existing = abs.as_path();
    let mut trailing: Vec<&OsStr> = Vec::new();
    let canon = loop {
        match existing.canonicalize() {
            Ok(c) => break c,
            Err(_) => match existing.parent() {
                Some(parent) => {
                    if let Some(name) = existing.file_name() {
                        trailing.push(name);
                    }
                    existing = parent;
                }
                None => {
                    return Err(mlua::Error::runtime(format!(
                        "fs: cannot resolve path '{path}'"
                    )));
                }
            },
        }
    };

    let mut result = canon;
    for name in trailing.iter().rev() {
        let comp = Path::new(name);
        match comp.components().next() {
            Some(Component::ParentDir) => {
                result.pop();
            }
            Some(Component::CurDir) | None => {}
            _ => result.push(name),
        }
    }
    Ok(result)
}

fn check_sandbox(path: &str, roots: &[PathBuf]) -> LuaResult<PathBuf> {
    let resolved = resolve_for_sandbox(path)?;
    if roots.iter().any(|r| resolved.starts_with(r)) {
        Ok(resolved)
    } else {
        Err(mlua::Error::runtime(format!("{SANDBOX_ERR}: {path}")))
    }
}

pub(crate) fn create_fs_table(lua: &Lua, roots: Arc<[PathBuf]>) -> LuaResult<Table> {
    let t = lua.create_table()?;

    let roots_read = Arc::clone(&roots);
    t.set(
        "read",
        lua.create_function(move |_, path: String| {
            let canonical = check_sandbox(&path, &roots_read)?;
            std::fs::read_to_string(&canonical).map_err(|e| {
                if e.kind() == ErrorKind::InvalidData {
                    mlua::Error::runtime("non-utf8 content; use read_bytes")
                } else {
                    mlua::Error::runtime(format!("fs.read({path}): {e}"))
                }
            })
        })?,
    )?;

    let roots_bytes = Arc::clone(&roots);
    t.set(
        "read_bytes",
        lua.create_function(move |lua, path: String| -> LuaResult<Buffer> {
            let canonical = check_sandbox(&path, &roots_bytes)?;
            let bytes = std::fs::read(&canonical)
                .map_err(|e| mlua::Error::runtime(format!("fs.read_bytes({path}): {e}")))?;
            lua.create_buffer(bytes)
        })?,
    )?;

    let roots_meta = Arc::clone(&roots);
    t.set(
        "metadata",
        lua.create_function(move |lua, path: String| -> LuaResult<Table> {
            let canonical = check_sandbox(&path, &roots_meta)?;
            let meta = std::fs::metadata(&canonical)
                .map_err(|e| mlua::Error::runtime(format!("fs.metadata({path}): {e}")))?;
            let tbl = lua.create_table()?;
            tbl.set("size", meta.len())?;
            tbl.set("is_file", meta.is_file())?;
            tbl.set("is_dir", meta.is_dir())?;
            Ok(tbl)
        })?,
    )?;

    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use tempfile::TempDir;

    fn roots(dirs: &[&Path]) -> Arc<[PathBuf]> {
        dirs.iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()))
            .collect::<Vec<_>>()
            .into()
    }

    #[test]
    fn fs_read_within_cwd_ok() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        std::fs::write(&file, "world").unwrap();

        let lua = Lua::new();
        let allowed = roots(&[tmp.path()]);
        let tbl = create_fs_table(&lua, allowed).unwrap();
        let read: mlua::Function = tbl.get("read").unwrap();
        let result: String = read.call(file.to_str().unwrap()).unwrap();
        assert_eq!(result, "world");
    }

    #[test]
    fn fs_read_outside_cwd_denied() {
        let tmp = TempDir::new().unwrap();

        let lua = Lua::new();
        let allowed = roots(&[tmp.path()]);
        let tbl = create_fs_table(&lua, allowed).unwrap();
        let read: mlua::Function = tbl.get("read").unwrap();
        let err = read
            .call::<String>("/etc/hostname")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(SANDBOX_ERR),
            "expected sandbox error, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fs_read_symlink_escape_denied() {
        let tmp = TempDir::new().unwrap();
        let link = tmp.path().join("escape");
        std::os::unix::fs::symlink("/etc/hostname", &link).unwrap();

        let lua = Lua::new();
        let allowed = roots(&[tmp.path()]);
        let tbl = create_fs_table(&lua, allowed).unwrap();

        let read: mlua::Function = tbl.get("read").unwrap();
        let err = read
            .call::<String>(link.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(SANDBOX_ERR),
            "expected sandbox error for symlink escape, got: {err}"
        );
    }

    #[test]
    fn fs_read_bytes_sandbox_check() {
        let tmp = TempDir::new().unwrap();

        let lua = Lua::new();
        let allowed = roots(&[tmp.path()]);
        let tbl = create_fs_table(&lua, allowed).unwrap();
        let read_bytes: mlua::Function = tbl.get("read_bytes").unwrap();
        let err = read_bytes
            .call::<Buffer>("/etc/hostname")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(SANDBOX_ERR),
            "expected sandbox error, got: {err}"
        );
    }

    #[test]
    fn fs_metadata_sandbox_check() {
        let tmp = TempDir::new().unwrap();

        let lua = Lua::new();
        let allowed = roots(&[tmp.path()]);
        let tbl = create_fs_table(&lua, allowed).unwrap();
        let metadata: mlua::Function = tbl.get("metadata").unwrap();
        let err = metadata
            .call::<Table>("/etc/hostname")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(SANDBOX_ERR),
            "expected sandbox error, got: {err}"
        );
    }
}
