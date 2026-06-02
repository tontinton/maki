use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs::FileType;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use mlua::{Lua, Result as LuaResult, Table};

use crate::plugin_permissions::{
    Permission::{FsRead, FsWrite},
    PluginPermissions,
};

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

fn io_result(lua: &Lua, result: std::io::Result<()>) -> LuaResult<(mlua::Value, mlua::Value)> {
    match result {
        Ok(()) => Ok((mlua::Value::Boolean(true), mlua::Value::Nil)),
        Err(e) => Ok((
            mlua::Value::Nil,
            mlua::Value::String(lua.create_string(e.to_string())?),
        )),
    }
}

pub(crate) fn create_fs_table(lua: &Lua, perms: &PluginPermissions) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "read",
        perms.guard_async(FsRead, lua, |_, path: String| async move {
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
        perms.guard_async(FsRead, lua, |lua, path: String| async move {
            let abs = make_absolute(&path)?;
            let bytes = smol::fs::read(&abs)
                .await
                .map_err(|e| mlua::Error::runtime(format!("fs.read_bytes({path}): {e}")))?;
            lua.create_buffer(bytes)
        })?,
    )?;

    t.set(
        "metadata",
        perms.guard_async(FsRead, lua, |lua, path: String| async move {
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
        perms.guard_async(
            FsRead,
            lua,
            |_, (source, marker): (String, mlua::Value)| async move {
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
            },
        )?,
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
        perms.guard_async(
            FsRead,
            lua,
            |lua, (path, opts): (String, Option<Table>)| async move {
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
            },
        )?,
    )?;

    t.set(
        "write",
        perms.guard_async(
            FsWrite,
            lua,
            |lua, (path, content): (String, String)| async move {
                let abs = make_absolute(&path)?;
                let result = smol::fs::write(&abs, content).await;
                io_result(&lua, result)
            },
        )?,
    )?;

    t.set(
        "rm",
        perms.guard_async(FsWrite, lua, |lua, path: String| async move {
            let abs = make_absolute(&path)?;
            let result = smol::fs::remove_file(&abs).await;
            io_result(&lua, result)
        })?,
    )?;

    t.set(
        "mkdir",
        perms.guard_async(
            FsWrite,
            lua,
            |lua, (path, opts): (String, Option<Table>)| async move {
                let abs = make_absolute(&path)?;
                let parents = opts
                    .as_ref()
                    .and_then(|t| t.get::<bool>("parents").ok())
                    .unwrap_or(false);
                let result = if parents {
                    smol::fs::create_dir_all(&abs).await
                } else {
                    smol::fs::create_dir(&abs).await
                };
                io_result(&lua, result)
            },
        )?,
    )?;

    t.set(
        "glob",
        perms.guard_async(
            FsRead,
            lua,
            |lua, (patterns, opts): (mlua::Value, Option<Table>)| async move {
                let patterns: Vec<String> = match patterns {
                    mlua::Value::String(s) => vec![s.to_str()?.to_owned()],
                    mlua::Value::Table(t) => {
                        let mut v = Vec::new();
                        for val in t.sequence_values::<String>() {
                            v.push(val?);
                        }
                        v
                    }
                    _ => {
                        return Err(mlua::Error::runtime(
                            "glob: patterns must be a string or array of strings",
                        ));
                    }
                };

                let path = opts.as_ref().and_then(|t| t.get::<String>("path").ok());
                let limit = opts.as_ref().and_then(|t| t.get::<usize>("limit").ok());
                let gitignore = opts
                    .as_ref()
                    .and_then(|t| t.get::<bool>("gitignore").ok())
                    .unwrap_or(true);
                let sort = opts.as_ref().and_then(|t| t.get::<String>("sort").ok());
                let sort_mtime = sort.as_deref() == Some("mtime");

                let results = smol::unblock(move || {
                    let root = maki_agent::tools::resolve_search_path(path.as_deref())
                        .map_err(mlua::Error::runtime)?;
                    let pattern_refs: Vec<&str> = patterns.iter().map(|s| s.as_str()).collect();

                    let walker =
                        maki_agent::tools::walk_builder_opts(&root, &pattern_refs, gitignore)
                            .map_err(mlua::Error::runtime)?
                            .build();

                    let iter = walker
                        .flatten()
                        .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()));

                    let paths: Vec<String> = if sort_mtime {
                        let mut entries: Vec<_> = iter
                            .filter_map(|e| {
                                let p = e.into_path();
                                let mt = maki_agent::tools::mtime(&p);
                                p.to_str().map(|s| (mt, s.to_owned()))
                            })
                            .collect();
                        entries.sort_unstable_by_key(|e| Reverse(e.0));
                        if let Some(lim) = limit {
                            entries.truncate(lim);
                        }
                        entries.into_iter().map(|(_, s)| s).collect()
                    } else {
                        let bounded: Box<dyn Iterator<Item = _>> = match limit {
                            Some(lim) => Box::new(iter.take(lim)),
                            None => Box::new(iter),
                        };
                        bounded
                            .filter_map(|e| e.into_path().to_str().map(|s| s.to_owned()))
                            .collect()
                    };

                    Ok::<_, mlua::Error>(paths)
                })
                .await?;

                let tbl = lua.create_table()?;
                for (i, path) in results.iter().enumerate() {
                    tbl.set(i + 1, path.as_str())?;
                }
                Ok(tbl)
            },
        )?,
    )?;

    Ok(t)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::plugin_permissions::PluginPermissions;
    use mlua::Lua;
    use tempfile::TempDir;

    #[test]
    fn read_file_ok() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        std::fs::write(&file, "world").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
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
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
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
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
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
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
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
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
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
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
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
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
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
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
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

    #[test]
    fn write_creates_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("new.txt");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let write: mlua::Function = tbl.get("write").unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            smol::block_on(write.call_async((file.to_str().unwrap(), "hello world"))).unwrap();
        assert!(
            matches!(ok, mlua::Value::Boolean(true)),
            "write should succeed"
        );
        assert!(matches!(err, mlua::Value::Nil));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[test]
    fn write_overwrites_existing() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("overwrite.txt");
        std::fs::write(&file, "old").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let write: mlua::Function = tbl.get("write").unwrap();
        smol::block_on(
            write.call_async::<(mlua::Value, mlua::Value)>((file.to_str().unwrap(), "new")),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
    }

    #[test]
    fn write_to_nonexistent_parent_returns_error() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("no_parent/deep/file.txt");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let write: mlua::Function = tbl.get("write").unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            smol::block_on(write.call_async((file.to_str().unwrap(), "data"))).unwrap();
        assert!(matches!(ok, mlua::Value::Nil), "should fail");
        assert!(
            matches!(err, mlua::Value::String(_)),
            "should return error string"
        );
    }

    #[test]
    fn rm_deletes_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("doomed.txt");
        std::fs::write(&file, "bye").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            smol::block_on(rm.call_async(file.to_str().unwrap())).unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(!file.exists());
    }

    #[test]
    fn rm_nonexistent_returns_error() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("ghost.txt");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            smol::block_on(rm.call_async(file.to_str().unwrap())).unwrap();
        assert!(
            matches!(ok, mlua::Value::Nil),
            "should fail for nonexistent"
        );
        assert!(matches!(err, mlua::Value::String(_)));
    }

    #[test]
    fn mkdir_creates_single_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("newdir");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let mkdir: mlua::Function = tbl.get("mkdir").unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            smol::block_on(mkdir.call_async(dir.to_str().unwrap())).unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(dir.is_dir());
    }

    #[test]
    fn mkdir_without_parents_fails_on_deep_path() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("a/b/c");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let mkdir: mlua::Function = tbl.get("mkdir").unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            smol::block_on(mkdir.call_async(dir.to_str().unwrap())).unwrap();
        assert!(
            matches!(ok, mlua::Value::Nil),
            "should fail without parents option"
        );
        assert!(matches!(err, mlua::Value::String(_)));
    }

    #[test]
    fn mkdir_with_parents_creates_nested() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("x/y/z");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let mkdir: mlua::Function = tbl.get("mkdir").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("parents", true).unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            smol::block_on(mkdir.call_async((dir.to_str().unwrap(), opts))).unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(dir.is_dir());
    }

    #[test]
    fn mkdir_already_exists_returns_error() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("exists");
        std::fs::create_dir(&dir).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let mkdir: mlua::Function = tbl.get("mkdir").unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            smol::block_on(mkdir.call_async(dir.to_str().unwrap())).unwrap();
        assert!(
            matches!(ok, mlua::Value::Nil),
            "creating existing dir should fail"
        );
        assert!(matches!(err, mlua::Value::String(_)));
    }

    #[test]
    fn mkdir_with_parents_idempotent() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("idem");
        std::fs::create_dir(&dir).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let mkdir: mlua::Function = tbl.get("mkdir").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("parents", true).unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            smol::block_on(mkdir.call_async((dir.to_str().unwrap(), opts))).unwrap();
        assert!(
            matches!(ok, mlua::Value::Boolean(true)),
            "parents=true should be idempotent"
        );
    }

    #[test]
    fn glob_finds_matching_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn main(){}").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "hello").unwrap();
        let dir_str = tmp.path().to_string_lossy().to_string();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", dir_str.as_str()).unwrap();

        let result: Table = smol::block_on(glob.call_async::<Table>(("*.rs", opts))).unwrap();

        let mut paths: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            paths.push(result.get::<String>(i).unwrap());
        }
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("a.rs"));
    }

    #[test]
    fn glob_multiple_patterns_union() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "").unwrap();
        std::fs::write(tmp.path().join("c.py"), "").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let patterns = lua.create_table().unwrap();
        patterns.set(1, "*.rs").unwrap();
        patterns.set(2, "*.txt").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();

        let result: Table = smol::block_on(glob.call_async::<Table>((patterns, opts))).unwrap();

        let mut paths: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            paths.push(result.get::<String>(i).unwrap());
        }
        paths.sort();
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("a.rs"));
        assert!(paths[1].ends_with("b.txt"));
    }

    #[test]
    fn glob_limit_caps_results() {
        let tmp = TempDir::new().unwrap();
        for i in 0..5 {
            std::fs::write(tmp.path().join(format!("f{i}.rs")), "").unwrap();
        }

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        opts.set("limit", 2).unwrap();

        let result: Table = smol::block_on(glob.call_async::<Table>(("*.rs", opts))).unwrap();
        assert_eq!(result.len().unwrap(), 2);
    }

    #[test]
    fn glob_no_matches_returns_empty_table() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();

        let result: Table = smol::block_on(glob.call_async::<Table>(("*.nope", opts))).unwrap();
        assert_eq!(result.len().unwrap(), 0);
    }

    #[test]
    fn glob_invalid_pattern_type_errors() {
        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let err = smol::block_on(glob.call_async::<Table>((mlua::Value::Integer(42), mlua::Nil)))
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("patterns must be a string or array of strings"),
        );
    }

    #[test]
    fn glob_mtime_sort_newest_first() {
        let tmp = TempDir::new().unwrap();
        let old_path = tmp.path().join("old.rs");
        let new_path = tmp.path().join("new.rs");
        std::fs::write(&old_path, "").unwrap();
        std::fs::write(&new_path, "").unwrap();

        let old_time = SystemTime::now() - Duration::from_secs(60);
        let new_time = SystemTime::now();
        std::fs::File::open(&old_path)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        std::fs::File::open(&new_path)
            .unwrap()
            .set_modified(new_time)
            .unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        opts.set("sort", "mtime").unwrap();

        let result: Table = smol::block_on(glob.call_async::<Table>(("*.rs", opts))).unwrap();

        let first: String = result.get(1).unwrap();
        let second: String = result.get(2).unwrap();
        assert!(first.ends_with("new.rs"));
        assert!(second.ends_with("old.rs"));
    }

    #[test]
    fn glob_no_opts_uses_cwd() {
        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let result = smol::block_on(glob.call_async::<Table>(("*.rs", mlua::Nil)));
        assert!(result.is_ok());
    }

    #[test]
    fn glob_path_option_scopes_to_directory() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner.rs"), "").unwrap();
        std::fs::write(tmp.path().join("outer.rs"), "").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", sub.to_str().unwrap()).unwrap();

        let result: Table = smol::block_on(glob.call_async::<Table>(("*.rs", opts))).unwrap();

        let mut paths: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            paths.push(result.get::<String>(i).unwrap());
        }
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("inner.rs"));
    }
}
