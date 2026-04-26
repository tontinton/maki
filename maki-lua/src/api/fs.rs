use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use mlua::{Lua, Result as LuaResult, Table};

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
        .ok_or_else(|| mlua::Error::runtime("non-utf8 path"))
}

pub(crate) fn create_fs_table(lua: &Lua) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "read",
        lua.create_async_function(|_, path: String| async move {
            let abs = make_absolute(&path)?;
            smol::fs::read_to_string(&abs).await.map_err(|e| {
                if e.kind() == ErrorKind::InvalidData {
                    mlua::Error::runtime("non-utf8 content; use read_bytes")
                } else {
                    mlua::Error::runtime(format!("fs.read({path}): {e}"))
                }
            })
        })?,
    )?;

    t.set(
        "read_bytes",
        lua.create_async_function(|lua, path: String| async move {
            let abs = make_absolute(&path)?;
            let bytes = smol::fs::read(&abs)
                .await
                .map_err(|e| mlua::Error::runtime(format!("fs.read_bytes({path}): {e}")))?;
            lua.create_buffer(bytes)
        })?,
    )?;

    t.set(
        "metadata",
        lua.create_async_function(|lua, path: String| async move {
            let abs = make_absolute(&path)?;
            let meta = smol::fs::metadata(&abs)
                .await
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
        lua.create_async_function(|_, (source, marker): (String, mlua::Value)| async move {
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

            smol::unblock(move || {
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
            })
            .await
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

    #[test]
    fn read_file_ok() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        std::fs::write(&file, "world").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua).unwrap();
        let read: mlua::Function = tbl.get("read").unwrap();
        let result: String = smol::block_on(read.call_async(file.to_str().unwrap())).unwrap();
        assert_eq!(result, "world");
    }
}
