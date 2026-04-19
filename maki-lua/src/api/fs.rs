use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use mlua::{Buffer, Lua, Result as LuaResult, Table};

const SANDBOX_ERR: &str = "path outside sandbox";
const NON_UTF8: &str = "non-utf8 path";

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

fn make_absolute(path: &str) -> LuaResult<PathBuf> {
    let p = expand_tilde(path);
    if p.is_absolute() {
        Ok(p)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&p))
            .map_err(|e| mlua::Error::runtime(format!("cannot resolve cwd: {e}")))
    }
}

fn path_to_string(p: &Path) -> LuaResult<String> {
    p.to_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| mlua::Error::runtime(NON_UTF8))
}

/// Canonicalize the deepest existing ancestor, then re-append the rest lexically.
/// This way plugins can write to paths that don't exist yet, but symlink escapes
/// through existing components are still caught.
pub(crate) fn resolve_for_sandbox(path: &str) -> LuaResult<PathBuf> {
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

pub(crate) fn check_sandbox(path: &str, roots: &[PathBuf]) -> LuaResult<PathBuf> {
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

    // vim.fs-compatible path utilities

    t.set(
        "dirname",
        lua.create_function(|_, file: String| {
            Ok(Path::new(&file)
                .parent()
                .and_then(|p| p.to_str())
                .map(|s| s.to_owned()))
        })?,
    )?;

    t.set(
        "basename",
        lua.create_function(|_, file: String| {
            Ok(Path::new(&file)
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_owned()))
        })?,
    )?;

    t.set(
        "joinpath",
        lua.create_function(|_, parts: mlua::Variadic<String>| {
            let mut buf = PathBuf::new();
            for part in parts.iter() {
                buf.push(part);
            }
            path_to_string(&buf)
        })?,
    )?;

    t.set(
        "normalize",
        lua.create_function(|_, path: String| {
            let abs = make_absolute(&path)?;
            let mut components = Vec::new();
            for comp in abs.components() {
                match comp {
                    Component::ParentDir => {
                        components.pop();
                    }
                    Component::CurDir => {}
                    _ => components.push(comp),
                }
            }
            let result: PathBuf = components.iter().collect();
            path_to_string(&result)
        })?,
    )?;

    t.set(
        "abspath",
        lua.create_function(|_, path: String| path_to_string(&make_absolute(&path)?))?,
    )?;

    t.set(
        "parents",
        lua.create_function(|lua, start: String| {
            let p = Path::new(&start);
            let tbl = lua.create_table()?;
            let mut i = 1;
            let mut current = p.parent();
            while let Some(parent) = current {
                if let Some(s) = parent.to_str() {
                    tbl.set(i, s)?;
                    i += 1;
                }
                current = parent.parent();
            }
            Ok(tbl)
        })?,
    )?;

    t.set(
        "root",
        lua.create_function(|_, (source, marker): (String, mlua::Value)| {
            let markers: Vec<String> = match marker {
                mlua::Value::String(s) => vec![s.to_str()?.to_owned()],
                mlua::Value::Table(t) => {
                    let mut v = Vec::new();
                    for pair in t.sequence_values::<String>() {
                        v.push(pair?);
                    }
                    v
                }
                _ => {
                    return Err(mlua::Error::runtime(
                        "fs.root: marker must be a string or list of strings",
                    ));
                }
            };

            let start = Path::new(&source);
            let start = if start.is_file() || !start.exists() {
                start.parent().unwrap_or(start)
            } else {
                start
            };

            let mut dir = make_absolute(start.to_str().unwrap_or_default())?;

            loop {
                for m in &markers {
                    if dir.join(m).exists() {
                        return Ok(Some(path_to_string(&dir)?));
                    }
                }
                if !dir.pop() {
                    return Ok(None);
                }
            }
        })?,
    )?;

    t.set(
        "relpath",
        lua.create_function(|_, (base, target): (String, String)| {
            let base_comps: Vec<_> = Path::new(&base).components().collect();
            let target_comps: Vec<_> = Path::new(&target).components().collect();

            let common = base_comps
                .iter()
                .zip(target_comps.iter())
                .take_while(|(a, b)| a == b)
                .count();

            let mut result = PathBuf::new();
            for _ in common..base_comps.len() {
                result.push("..");
            }
            for comp in &target_comps[common..] {
                result.push(comp);
            }
            path_to_string(&result)
        })?,
    )?;

    t.set(
        "ext",
        lua.create_function(|_, file: String| {
            Ok(Path::new(&file)
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_owned()))
        })?,
    )?;

    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use tempfile::TempDir;
    use test_case::test_case;

    fn roots(dirs: &[&Path]) -> Arc<[PathBuf]> {
        dirs.iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()))
            .collect::<Vec<_>>()
            .into()
    }

    #[test]
    fn read_within_sandbox_ok() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        std::fs::write(&file, "world").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, roots(&[tmp.path()])).unwrap();
        let read: mlua::Function = tbl.get("read").unwrap();
        let result: String = read.call(file.to_str().unwrap()).unwrap();
        assert_eq!(result, "world");
    }

    #[test_case("read"     ; "read")]
    #[test_case("read_bytes" ; "read_bytes")]
    #[test_case("metadata" ; "metadata")]
    fn sandbox_denies_outside_path(fn_name: &str) {
        let tmp = TempDir::new().unwrap();
        let lua = Lua::new();
        let tbl = create_fs_table(&lua, roots(&[tmp.path()])).unwrap();
        let f: mlua::Function = tbl.get(fn_name).unwrap();
        let err = f
            .call::<mlua::Value>("/etc/hostname")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(SANDBOX_ERR),
            "{fn_name}: expected sandbox error, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_denied() {
        let tmp = TempDir::new().unwrap();
        let link = tmp.path().join("escape");
        std::os::unix::fs::symlink("/etc/hostname", &link).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, roots(&[tmp.path()])).unwrap();
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
}
