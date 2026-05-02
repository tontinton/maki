use std::collections::HashSet;
use std::fs::FileType;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use mlua::{Lua, Result as LuaResult, Table};

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = maki_storage::paths::home() {
            return home.join(rest);
        }
    } else if path == "~" {
        if let Some(home) = maki_storage::paths::home() {
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

fn filetype_str(ft: &FileType) -> &'static str {
    if ft.is_file() {
        "file"
    } else if ft.is_dir() {
        "directory"
    } else if ft.is_symlink() {
        "link"
    } else {
        "unknown"
    }
}

fn collect_dir_entries(
    base: &Path,
    dir: &Path,
    depth: u32,
    max_depth: u32,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<(String, &'static str)>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.strip_prefix(base).ok().and_then(|p| p.to_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let (type_str, is_dir) = match entry.file_type() {
            Ok(ft) if ft.is_symlink() => match std::fs::metadata(&path) {
                Ok(meta) => (filetype_str(&meta.file_type()), meta.is_dir()),
                Err(_) => ("link", false),
            },
            Ok(ft) => (filetype_str(&ft), ft.is_dir()),
            Err(_) => ("unknown", false),
        };
        out.push((name, type_str));
        if is_dir && depth < max_depth {
            let canonical = match path.canonicalize() {
                Ok(c) => c,
                Err(_) => continue,
            };
            if visited.insert(canonical) {
                collect_dir_entries(base, &path, depth + 1, max_depth, visited, out);
            }
        }
    }
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
            match smol::fs::metadata(&abs).await {
                Ok(meta) => {
                    let tbl = lua.create_table()?;
                    tbl.set("size", meta.len())?;
                    tbl.set("is_file", meta.is_file())?;
                    tbl.set("is_dir", meta.is_dir())?;
                    Ok(mlua::Value::Table(tbl))
                }
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(mlua::Value::Nil),
                Err(e) => Err(mlua::Error::runtime(format!("fs.metadata({path}): {e}"))),
            }
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

    t.set(
        "dir",
        lua.create_async_function(|lua, (path, opts): (String, Option<Table>)| async move {
            let abs = make_absolute(&path)?;
            let max_depth: u32 = match &opts {
                Some(t) => t.get::<u32>("depth").unwrap_or(1),
                None => 1,
            };

            let entries = smol::unblock(move || {
                if !abs.exists() {
                    return Vec::new();
                }
                let mut out = Vec::new();
                let mut visited = HashSet::new();
                collect_dir_entries(&abs, &abs, 1, max_depth, &mut visited, &mut out);
                out
            })
            .await;

            let result = lua.create_table()?;
            for (i, (name, typ)) in entries.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set(1, name.as_str())?;
                entry.set(2, *typ)?;
                result.set(i + 1, entry)?;
            }
            Ok(mlua::Value::Table(result))
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

    #[test]
    fn dir_lists_entries() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();
        let result: Table =
            smol::block_on(dir.call_async::<Table>(tmp.path().to_str().unwrap())).unwrap();

        let mut names: Vec<String> = Vec::new();
        let mut types: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            let entry: Table = result.get(i).unwrap();
            names.push(entry.get::<String>(1).unwrap());
            types.push(entry.get::<String>(2).unwrap());
        }
        names.sort();
        assert_eq!(names, vec!["a.txt", "sub"]);
        assert!(types.contains(&"file".to_owned()));
        assert!(types.contains(&"directory".to_owned()));
    }

    #[test]
    fn dir_recursive() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("d")).unwrap();
        std::fs::write(tmp.path().join("d/nested.txt"), "").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("depth", 2).unwrap();

        let result: Table =
            smol::block_on(dir.call_async::<Table>((tmp.path().to_str().unwrap(), opts))).unwrap();

        let mut names: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            let entry: Table = result.get(i).unwrap();
            names.push(entry.get::<String>(1).unwrap());
        }
        names.sort();
        assert!(names.contains(&"d".to_owned()));
        assert!(names.iter().any(|n| n.contains("nested.txt")));
    }

    #[test]
    fn dir_nonexistent_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let lua = Lua::new();
        let tbl = create_fs_table(&lua).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();
        let missing = tmp.path().join("does_not_exist");
        let result: Table =
            smol::block_on(dir.call_async::<Table>(missing.to_str().unwrap())).unwrap();
        assert_eq!(result.len().unwrap(), 0);
    }

    #[test]
    fn metadata_file_dir_and_missing() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("probe.txt");
        std::fs::write(&file, "hello").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua).unwrap();
        let metadata: mlua::Function = tbl.get("metadata").unwrap();

        let f: Table =
            smol::block_on(metadata.call_async::<Table>(file.to_str().unwrap())).unwrap();
        assert!(f.get::<bool>("is_file").unwrap());
        assert!(!f.get::<bool>("is_dir").unwrap());
        assert_eq!(f.get::<u64>("size").unwrap(), 5);

        let d: Table =
            smol::block_on(metadata.call_async::<Table>(tmp.path().to_str().unwrap())).unwrap();
        assert!(!d.get::<bool>("is_file").unwrap());
        assert!(d.get::<bool>("is_dir").unwrap());

        let missing = tmp.path().join("nope");
        let nil: mlua::Value =
            smol::block_on(metadata.call_async(missing.to_str().unwrap())).unwrap();
        assert!(matches!(nil, mlua::Value::Nil));
    }

    #[cfg(unix)]
    #[test]
    fn dir_follows_symlinks() {
        let tmp = TempDir::new().unwrap();
        let real_dir = tmp.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        std::fs::write(real_dir.join("inner.txt"), "").unwrap();
        std::os::unix::fs::symlink(&real_dir, tmp.path().join("link")).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("depth", 2u32).unwrap();

        let result: Table =
            smol::block_on(dir.call_async::<Table>((tmp.path().to_str().unwrap(), opts))).unwrap();

        let mut names: Vec<String> = Vec::new();
        let mut types: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            let entry: Table = result.get(i).unwrap();
            names.push(entry.get::<String>(1).unwrap());
            types.push(entry.get::<String>(2).unwrap());
        }

        assert!(names.iter().any(|n| n.contains("inner.txt")));
        let link_idx = names.iter().position(|n| n == "link").unwrap();
        assert_eq!(types[link_idx], "directory");
    }

    #[cfg(unix)]
    #[test]
    fn dir_dangling_symlink() {
        let tmp = TempDir::new().unwrap();
        std::os::unix::fs::symlink("/nonexistent_target_xyz", tmp.path().join("broken")).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();

        let result: Table =
            smol::block_on(dir.call_async::<Table>(tmp.path().to_str().unwrap())).unwrap();

        let mut found = false;
        for i in 1..=result.len().unwrap() {
            let entry: Table = result.get(i).unwrap();
            let name: String = entry.get::<String>(1).unwrap();
            if name == "broken" {
                let typ: String = entry.get::<String>(2).unwrap();
                assert_eq!(typ, "link");
                found = true;
            }
        }
        assert!(found, "dangling symlink should still appear in listing");
    }

    #[cfg(unix)]
    #[test]
    fn dir_symlink_cycle_does_not_loop() {
        let tmp = TempDir::new().unwrap();
        let child = tmp.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::os::unix::fs::symlink(tmp.path(), child.join("loop")).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("depth", 10u32).unwrap();

        let result: Table =
            smol::block_on(dir.call_async::<Table>((tmp.path().to_str().unwrap(), opts))).unwrap();

        let len = result.len().unwrap();
        assert!(
            len < 20,
            "symlink cycle produced {len} entries, expected bounded"
        );
    }
}
