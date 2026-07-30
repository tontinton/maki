use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs::FileType;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{IntoLua, Lua, Result as LuaResult, Table, Value};

use crate::api::util::convert::err_pair;
use crate::plugin_permissions::PluginPermissions;

pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = maki_storage::paths::home() {
            return home.join(rest);
        }
    } else if path == "~"
        && let Some(home) = maki_storage::paths::home()
    {
        return home;
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

fn result_pair<T: mlua::IntoLua, E: std::fmt::Display>(
    lua: &Lua,
    result: Result<T, E>,
) -> LuaResult<(mlua::Value, mlua::Value)> {
    match result {
        Ok(val) => Ok((val.into_lua(lua)?, mlua::Value::Nil)),
        Err(e) => Ok((
            mlua::Value::Nil,
            mlua::Value::String(lua.create_string(e.to_string())?),
        )),
    }
}

/// Read the entire file at {path} as a UTF-8 string.
/// If the file contains bytes that are not valid UTF-8, this function throws.
/// Use `read_bytes` for binary files.
///
/// @param path string Absolute or relative file path. `~/` is expanded to the home directory.
/// @return (string?, string?) File contents, or nil plus an error message.
/// @example
/// local text, err = maki.fs.read("config.toml")
/// if err then
///   maki.log.warn("could not read config: " .. err)
///   return
/// end
#[lua_fn(guard = FsRead)]
async fn read(lua: Lua, path: String) -> LuaResult<(Value, Value)> {
    let abs = make_absolute(&path)?;
    match smol::fs::read_to_string(&abs).await {
        Ok(s) => Ok((s.into_lua(&lua)?, Value::Nil)),
        Err(e) if e.kind() == ErrorKind::InvalidData => {
            Err(mlua::Error::runtime("non-utf8 content; use read_bytes"))
        }
        Err(e) => Ok((Value::Nil, Value::String(lua.create_string(e.to_string())?))),
    }
}

/// Read the entire file at {path} as raw bytes, returned as a Luau buffer.
/// Useful for binary files or when you need to pass the data to `maki.base64.encode`.
///
/// @param path string Absolute or relative file path. `~/` is expanded to the home directory.
/// @return (buffer?, string?) File bytes as a Luau buffer, or nil plus an error message.
/// @example
/// local buf, err = maki.fs.read_bytes("image.png")
/// if err then return end
/// local encoded = maki.base64.encode(buf)
#[lua_fn(guard = FsRead)]
async fn read_bytes(lua: Lua, path: String) -> LuaResult<(Value, Value)> {
    let abs = make_absolute(&path)?;
    match smol::fs::read(&abs).await {
        Ok(bytes) => Ok((lua.create_buffer(bytes)?.into_lua(&lua)?, Value::Nil)),
        Err(e) => Ok((Value::Nil, Value::String(lua.create_string(e.to_string())?))),
    }
}

/// Get metadata for the file or directory at {path}.
/// Returns a table with `size` (integer), `is_file` (boolean), `is_dir` (boolean),
/// and `mtime` (number, fractional seconds since the Unix epoch; absent when the
/// filesystem does not report a modification time).
/// If {path} does not exist, returns nil with no error.
///
/// @param path string Absolute or relative path.
/// @return (table?, string?) Metadata table, nil if missing, or nil plus an error message.
/// @example
/// local meta = maki.fs.metadata("src/main.rs")
/// if meta and meta.is_file then
///   print("size: " .. meta.size)
/// end
#[lua_fn(guard = FsRead)]
async fn metadata(lua: Lua, path: String) -> LuaResult<(Value, Value)> {
    let abs = make_absolute(&path)?;
    match smol::fs::metadata(&abs).await {
        Ok(meta) => {
            let tbl = lua.create_table()?;
            tbl.set("size", meta.len())?;
            tbl.set("is_file", meta.is_file())?;
            tbl.set("is_dir", meta.is_dir())?;
            if let Ok(modified) = meta.modified()
                && let Ok(dur) = modified.duration_since(UNIX_EPOCH)
            {
                tbl.set("mtime", dur.as_secs_f64())?;
            }
            Ok((Value::Table(tbl), Value::Nil))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok((Value::Nil, Value::Nil)),
        Err(e) => Ok((Value::Nil, Value::String(lua.create_string(e.to_string())?))),
    }
}

/// Return the parent directory of {path}. Like `vim.fs.dirname`.
///
/// @param path string File path.
/// @return (string?) Parent directory, or nil if {path} has no parent.
/// @example
/// maki.fs.dirname("/home/user/init.lua") -- "/home/user"
#[lua_fn]
fn dirname(_lua: &Lua, path: String) -> LuaResult<Option<String>> {
    Ok(Path::new(&path)
        .parent()
        .and_then(|p| p.to_str())
        .map(|s| s.to_owned()))
}

/// Return the final component (the file name) of {path}. Like `vim.fs.basename`.
///
/// @param path string File path.
/// @return (string?) File name, or nil for paths like `/`.
/// @example
/// maki.fs.basename("/home/user/init.lua") -- "init.lua"
#[lua_fn]
fn basename(_lua: &Lua, path: String) -> LuaResult<Option<String>> {
    Ok(Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_owned()))
}

/// Join one or more path segments into a single path. Like `vim.fs.joinpath`.
///
/// @param parts string One or more path segments to join.
/// @return (string) The joined path.
/// @example
/// maki.fs.joinpath("src", "api", "fs.rs") -- "src/api/fs.rs"
#[lua_fn]
fn joinpath(_lua: &Lua, parts: mlua::Variadic<String>) -> LuaResult<String> {
    let mut buf = PathBuf::new();
    for part in parts.iter() {
        buf.push(part);
    }
    path_to_string(&buf)
}

/// Clean up `.` and `..` segments and make {path} absolute. Like `vim.fs.normalize`.
/// This is purely string-based and does not touch the filesystem.
///
/// @param path string Path to normalize. `~/` is expanded.
/// @return (string) Normalized absolute path.
/// @example
/// maki.fs.normalize("src/../src/api") -- "/home/user/project/src/api"
#[lua_fn]
fn normalize(_lua: &Lua, path: String) -> LuaResult<String> {
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
}

/// Make {path} absolute by prepending the current working directory when needed.
/// Unlike `normalize`, this does not resolve `.` or `..` segments.
///
/// @param path string Relative or absolute path. `~/` is expanded.
/// @return (string) Absolute path.
/// @example
/// maki.fs.abspath("src/main.rs") -- "/home/user/project/src/main.rs"
#[lua_fn]
fn abspath(_lua: &Lua, path: String) -> LuaResult<String> {
    path_to_string(&make_absolute(&path)?)
}

/// Return all ancestor directories of {path}, from the immediate parent up to the root.
/// Handy for walking up a directory tree.
///
/// @param path string File or directory path.
/// @return (string[]) Array of ancestor directory paths.
/// @example
/// local dirs = maki.fs.parents("/home/user/project/src")
/// -- { "/home/user/project", "/home/user", "/home", "/" }
#[lua_fn]
fn parents(lua: &Lua, path: String) -> LuaResult<Table> {
    let p = Path::new(&path);
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
}

/// Walk upward from {source} looking for a directory that contains one of the
/// {marker} files or directories. Like `vim.fs.root`. Useful for finding the
/// project root.
///
/// @param source string Starting file or directory path.
/// @param marker string|string[] Marker filename(s) to look for, e.g. `".git"` or `{"package.json", ".git"}`.
/// @return (string?, string?) Root directory path, or nil when not found.
/// @example
/// local root = maki.fs.root("src/main.rs", { ".git", "Cargo.toml" })
/// if root then print("project root: " .. root) end
#[lua_fn(guard = FsRead)]
async fn root(_lua: Lua, source: String, marker: Value) -> LuaResult<Option<String>> {
    let markers: Vec<String> = match marker {
        Value::String(s) => vec![s.to_str()?.to_owned()],
        Value::Table(t) => {
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
}

/// Compute a relative path from {base} to {target}.
///
/// @param base string Base directory path.
/// @param target string Target path.
/// @return (string) Relative path from {base} to {target}.
/// @example
/// maki.fs.relpath("/home/user", "/home/user/project/src") -- "project/src"
#[lua_fn]
fn relpath(_lua: &Lua, base: String, target: String) -> LuaResult<String> {
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
}

/// Return the file extension of {path}, without the leading dot.
///
/// @param path string File path.
/// @return (string?) Extension, or nil if the path has no extension.
/// @example
/// maki.fs.ext("main.rs")   -- "rs"
/// maki.fs.ext("Makefile")  -- nil
#[lua_fn]
fn ext(_lua: &Lua, path: String) -> LuaResult<Option<String>> {
    Ok(Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_owned()))
}

/// List the contents of the directory at {path}.
/// Each entry is a two-element array `{name, type}` where type is one of
/// `"file"`, `"directory"`, `"link"`, or `"unknown"`. Follows symlinks.
///
/// @param path string Directory path.
/// @param opts table? `depth` (integer, default 1): how many levels deep to recurse.
/// @return (table?, string?) Array of `{name, type}` entries, or nil plus an error message.
/// @example
/// local entries, err = maki.fs.dir("src", { depth = 2 })
/// if err then return end
/// for _, e in ipairs(entries) do
///   print(e[1], e[2]) -- "main.rs"  "file"
/// end
#[lua_fn(guard = FsRead)]
async fn dir(lua: Lua, path: String, opts: Option<Table>) -> LuaResult<(Value, Value)> {
    let abs = make_absolute(&path)?;
    let max_depth: u32 = match &opts {
        Some(t) => t.get::<u32>("depth").unwrap_or(1),
        None => 1,
    };

    let result = smol::unblock(move || -> Result<Vec<(String, &'static str)>, String> {
        if !abs.exists() {
            return Err(format!("dir: path does not exist: {}", abs.display()));
        }
        if !abs.is_dir() {
            return Err(format!("dir: not a directory: {}", abs.display()));
        }
        let mut out = Vec::new();
        let mut visited = HashSet::new();
        collect_dir_entries(&abs, &abs, 1, max_depth, &mut visited, &mut out);
        Ok(out)
    })
    .await;

    match result {
        Ok(entries) => {
            let tbl = lua.create_table()?;
            for (i, (name, typ)) in entries.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set(1, name.as_str())?;
                entry.set(2, *typ)?;
                tbl.set(i + 1, entry)?;
            }
            Ok((Value::Table(tbl), Value::Nil))
        }
        Err(e) => err_pair(&lua, e),
    }
}

/// Write {content} to the file at {path}, creating it if it does not exist
/// or overwriting it if it does.
///
/// @param path string Destination file path. `~/` is expanded.
/// @param content string Text to write.
/// @return (true?, string?) `true` on success, or nil plus an error message.
/// @example
/// local ok, err = maki.fs.write("out.txt", "hello world")
/// if err then print("write failed: " .. err) end
#[lua_fn(guard = FsWrite)]
async fn write(lua: Lua, path: String, content: String) -> LuaResult<(Value, Value)> {
    let abs = make_absolute(&path)?;
    result_pair(&lua, smol::fs::write(&abs, content).await.map(|()| true))
}

/// Delete the file, symlink, or directory at {path}.
/// Pass `recursive = true` to remove a non-empty directory tree (like `rm -r`).
/// Unlike `vim.fs.rm`, this also removes an empty directory without `recursive`.
/// Symlinks are removed themselves, never followed.
///
/// @param path string Path to the file or directory to remove.
/// @param opts table? `recursive` (boolean, default false): remove a directory and its contents recursively. `force` (boolean, default false): silently ignore a missing path.
/// @return (true?, string?) `true` on success, or nil plus an error message.
/// @example
/// local ok, err = maki.fs.rm("temp.txt")
/// if err then print("rm failed: " .. err) end
/// maki.fs.rm("stale_dir", { recursive = true, force = true })
#[lua_fn(guard = FsWrite)]
async fn rm(lua: Lua, path: String, opts: Option<Table>) -> LuaResult<(Value, Value)> {
    let abs = make_absolute(&path)?;
    let recursive = opts
        .as_ref()
        .and_then(|t| t.get::<bool>("recursive").ok())
        .unwrap_or(false);
    let force = opts
        .as_ref()
        .and_then(|t| t.get::<bool>("force").ok())
        .unwrap_or(false);
    let result = smol::unblock(move || -> std::io::Result<()> {
        let meta = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if force && e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if meta.is_dir() {
            if recursive {
                std::fs::remove_dir_all(&abs)
            } else {
                std::fs::remove_dir(&abs)
            }
        } else {
            match std::fs::remove_file(&abs) {
                Ok(()) => Ok(()),
                Err(e) if meta.file_type().is_symlink() => std::fs::remove_dir(&abs).map_err(|_| e),
                Err(e) => Err(e),
            }
        }
    })
    .await;
    result_pair(&lua, result.map(|()| true))
}

/// Create the directory at {path}. Set `parents = true` to create
/// intermediate directories, like `mkdir -p`.
///
/// @param path string Directory path to create.
/// @param opts table? `parents` (boolean, default false): create intermediate parent directories.
/// @return (true?, string?) `true` on success, or nil plus an error message.
/// @example
/// maki.fs.mkdir("a/b/c", { parents = true })
#[lua_fn(guard = FsWrite)]
async fn mkdir(lua: Lua, path: String, opts: Option<Table>) -> LuaResult<(Value, Value)> {
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
    result_pair(&lua, result.map(|()| true))
}

/// Find files matching one or more glob patterns.
/// Respects `.gitignore` by default. Pass `sort = "mtime"` to get the most
/// recently modified files first.
///
/// @param pattern string|string[] Glob pattern or array of patterns.
/// @param opts table? `path` (string): search root. `limit` (integer): max results. `gitignore` (boolean, default true): respect .gitignore. `sort` (string): `"mtime"` sorts newest first.
/// @return (string[]?, string?) Array of absolute file paths, or nil plus an error message.
/// @example
/// local files, err = maki.fs.glob("**/*.lua", { path = "plugins", limit = 10 })
/// if err then return end
/// for _, f in ipairs(files) do print(f) end
#[lua_fn(guard = FsRead)]
async fn glob(lua: Lua, pattern: Value, opts: Option<Table>) -> LuaResult<(Value, Value)> {
    let patterns: Vec<String> = match pattern {
        Value::String(s) => vec![s.to_str()?.to_owned()],
        Value::Table(t) => {
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

    let result: Result<Vec<String>, String> = smol::unblock(move || {
        let root = maki_agent::tools::resolve_search_path(path.as_deref())?;
        let pattern_refs: Vec<&str> = patterns.iter().map(|s| s.as_str()).collect();

        let walker = maki_agent::tools::walk_builder_opts(&root, &pattern_refs, gitignore)?.build();

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

        Ok(paths)
    })
    .await;

    match result {
        Ok(paths) => {
            let tbl = lua.create_table()?;
            for (i, path) in paths.iter().enumerate() {
                tbl.set(i + 1, path.as_str())?;
            }
            Ok((Value::Table(tbl), Value::Nil))
        }
        Err(e) => err_pair(&lua, format_args!("glob: {e}")),
    }
}

/// Search file contents for a regex {pattern}. Returns structured matches
/// grouped by file, similar to ripgrep output.
///
/// Each result entry has a `path` and a list of `groups`. Each group contains
/// `lines`, where every line has `line_nr`, `text`, and `is_match`.
///
/// @param pattern string Regular expression to search for.
/// @param opts table? `path` (string): search root. `include` (string): file glob filter (e.g. `"*.rs"`). `context_before` / `context_after` (integer): context lines around matches. `limit` (integer): max match groups. `max_line_bytes` (integer): skip lines longer than this.
/// @return (table?, string?) Array of `{path, groups}` tables, or nil plus an error message.
/// @example
/// local hits, err = maki.fs.grep("TODO", { path = "src", include = "*.rs", limit = 5 })
/// if err then return end
/// for _, file in ipairs(hits) do
///   for _, g in ipairs(file.groups) do
///     for _, line in ipairs(g.lines) do
///       if line.is_match then print(file.path .. ":" .. line.line_nr) end
///     end
///   end
/// end
#[lua_fn(guard = FsRead)]
async fn grep(lua: Lua, pattern: String, opts: Option<Table>) -> LuaResult<(Value, Value)> {
    let mut params = maki_agent::tools::grep::GrepParams::new(pattern);
    if let Some(ref opts) = opts {
        if let Ok(v) = opts.get::<String>("path") {
            params.path = Some(v);
        }
        if let Ok(v) = opts.get::<String>("include") {
            params.include = Some(v);
        }
        if let Ok(v) = opts.get::<usize>("context_before") {
            params.context_before = v;
        }
        if let Ok(v) = opts.get::<usize>("context_after") {
            params.context_after = v;
        }
        if let Ok(v) = opts.get::<usize>("limit") {
            params.limit = v;
        }
        if let Ok(v) = opts.get::<usize>("max_line_bytes") {
            params.max_line_bytes = v;
        }
    }

    let result = smol::unblock(move || maki_agent::tools::grep::grep_search(params)).await;

    match result {
        Ok((base, entries)) => {
            let arr = lua.create_table()?;
            for (i, entry) in entries.iter().enumerate() {
                let etbl = lua.create_table()?;
                etbl.set("path", base.join(&entry.path).to_string_lossy().as_ref())?;
                let groups_tbl = lua.create_table()?;
                for (gi, group) in entry.groups.iter().enumerate() {
                    let gtbl = lua.create_table()?;
                    let lines_tbl = lua.create_table()?;
                    for (li, line) in group.lines.iter().enumerate() {
                        let ltbl = lua.create_table()?;
                        ltbl.set("line_nr", line.line_nr)?;
                        ltbl.set("text", line.text.as_str())?;
                        ltbl.set("is_match", line.is_match)?;
                        lines_tbl.set(li + 1, ltbl)?;
                    }
                    gtbl.set("lines", lines_tbl)?;
                    groups_tbl.set(gi + 1, gtbl)?;
                }
                etbl.set("groups", groups_tbl)?;
                arr.set(i + 1, etbl)?;
            }
            Ok((Value::Table(arr), Value::Nil))
        }
        Err(e) => Ok((Value::Nil, Value::String(lua.create_string(e)?))),
    }
}

lua_table! {
    /// File-system utilities, modelled after `vim.fs` and `vim.uv`.
    ///
    /// Fallible operations return `(value, err)` pairs and never throw.
    /// Paths support `~/` expansion. Relative paths resolve from the current working directory.
    ///
    /// ```lua
    /// local text, err = maki.fs.read("init.lua")
    /// if err then return end
    /// ```
    "maki.fs" => pub(crate) fn create_fs_table(perms: &PluginPermissions), DOCS [
        read(perms), read_bytes(perms), metadata(perms), dirname, basename,
        joinpath, normalize, abspath, parents, root(perms), relpath, ext,
        dir(perms), write(perms), rm(perms), mkdir(perms), glob(perms), grep(perms),
    ]
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
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
    fn read_missing_returns_nil_err() {
        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();

        for func_name in ["read", "read_bytes"] {
            let f: mlua::Function = tbl.get(func_name).unwrap();
            let (val, err): (mlua::Value, mlua::Value) =
                smol::block_on(f.call_async("/nonexistent/path")).unwrap();
            assert_eq!(val, mlua::Value::Nil, "{func_name} should return nil");
            assert!(
                matches!(err, mlua::Value::String(_)),
                "{func_name} should return error"
            );
        }
    }

    #[test]
    fn dir_lists_entries() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();
        let (result, err): (Table, mlua::Value) =
            smol::block_on(dir.call_async::<(Table, mlua::Value)>(tmp.path().to_str().unwrap()))
                .unwrap();
        assert!(matches!(err, mlua::Value::Nil), "dir should succeed");

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

        let (result, err): (Table, mlua::Value) = smol::block_on(
            dir.call_async::<(Table, mlua::Value)>((tmp.path().to_str().unwrap(), opts)),
        )
        .unwrap();
        assert!(matches!(err, mlua::Value::Nil));

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
    fn dir_nonexistent_returns_nil_err() {
        let tmp = TempDir::new().unwrap();
        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();
        let missing = tmp.path().join("does_not_exist");
        let (val, err): (mlua::Value, mlua::Value) =
            smol::block_on(dir.call_async::<(mlua::Value, mlua::Value)>(missing.to_str().unwrap()))
                .unwrap();
        assert_eq!(
            val,
            mlua::Value::Nil,
            "dir should return nil for nonexistent path"
        );
        assert!(
            matches!(err, mlua::Value::String(_)),
            "dir should return error for nonexistent path"
        );
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
        assert!(f.get::<f64>("mtime").unwrap() > 0.0);

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

        let (result, err): (Table, mlua::Value) = smol::block_on(
            dir.call_async::<(Table, mlua::Value)>((tmp.path().to_str().unwrap(), opts)),
        )
        .unwrap();
        assert!(matches!(err, mlua::Value::Nil));

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

        let (result, err): (Table, mlua::Value) =
            smol::block_on(dir.call_async::<(Table, mlua::Value)>(tmp.path().to_str().unwrap()))
                .unwrap();
        assert!(matches!(err, mlua::Value::Nil), "dir should succeed");

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

        let (result, err): (Table, mlua::Value) = smol::block_on(
            dir.call_async::<(Table, mlua::Value)>((tmp.path().to_str().unwrap(), opts)),
        )
        .unwrap();
        assert!(matches!(err, mlua::Value::Nil));

        let len = result.len().unwrap();
        assert!(
            len < 20,
            "symlink cycle produced {len} entries, expected bounded"
        );
    }

    #[test]
    fn write_and_overwrite() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("new.txt");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let write: mlua::Function = tbl.get("write").unwrap();

        let (ok, err): (mlua::Value, mlua::Value) =
            smol::block_on(write.call_async((file.to_str().unwrap(), "first"))).unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(matches!(err, mlua::Value::Nil));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "first");

        smol::block_on(
            write.call_async::<(mlua::Value, mlua::Value)>((file.to_str().unwrap(), "second")),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "second");
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
    fn rm_force_ignores_missing() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("ghost.txt");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("force", true).unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            smol::block_on(rm.call_async((file.to_str().unwrap(), opts))).unwrap();
        assert!(
            matches!(ok, mlua::Value::Boolean(true)),
            "force should suppress NotFound"
        );
        assert!(matches!(err, mlua::Value::Nil));
    }

    #[test]
    fn rm_force_ignores_missing_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("never_existed");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("recursive", true).unwrap();
        opts.set("force", true).unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            smol::block_on(rm.call_async((dir.to_str().unwrap(), opts))).unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(matches!(err, mlua::Value::Nil));
    }

    #[test]
    fn rm_empty_dir_without_recursive() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("emptydir");
        std::fs::create_dir(&dir).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            smol::block_on(rm.call_async(dir.to_str().unwrap())).unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(!dir.exists());
    }

    #[test]
    fn rm_nonempty_dir_without_recursive_fails() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("nonempty");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("child.txt"), "x").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            smol::block_on(rm.call_async(dir.to_str().unwrap())).unwrap();
        assert!(
            matches!(ok, mlua::Value::Nil),
            "should fail without recursive"
        );
        assert!(matches!(err, mlua::Value::String(_)));
        assert!(dir.exists(), "non-empty dir should still exist");
    }

    #[test]
    fn rm_recursive_removes_tree() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("tree");
        std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "b").unwrap();
        std::fs::write(dir.join("sub/deeper/c.txt"), "c").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("recursive", true).unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            smol::block_on(rm.call_async((dir.to_str().unwrap(), opts))).unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rm_symlink_removes_link_not_target() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target.txt");
        std::fs::write(&target, "data").unwrap();
        let link = tmp.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            smol::block_on(rm.call_async(link.to_str().unwrap())).unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(!link.exists(), "symlink should be removed");
        assert!(target.exists(), "target should remain");
    }

    #[cfg(unix)]
    #[test]
    fn rm_recursive_symlink_to_dir_does_not_follow() {
        let tmp = TempDir::new().unwrap();
        let real_dir = tmp.path().join("real");
        std::fs::create_dir_all(real_dir.join("sub")).unwrap();
        std::fs::write(real_dir.join("sub/keep.txt"), "data").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("recursive", true).unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            smol::block_on(rm.call_async((link.to_str().unwrap(), opts))).unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(!link.exists(), "symlink should be removed");
        assert!(real_dir.exists(), "target dir should remain");
        assert!(
            real_dir.join("sub/keep.txt").exists(),
            "target dir contents should remain"
        );
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

        let (result, err): (Table, mlua::Value) =
            smol::block_on(glob.call_async::<(Table, mlua::Value)>(("*.rs", opts))).unwrap();
        assert!(matches!(err, mlua::Value::Nil));

        let mut paths: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            paths.push(result.get::<String>(i).unwrap());
        }
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("a.rs"));

        let opts2 = lua.create_table().unwrap();
        opts2.set("path", dir_str.as_str()).unwrap();
        let (empty, err2): (Table, mlua::Value) =
            smol::block_on(glob.call_async::<(Table, mlua::Value)>(("*.nope", opts2))).unwrap();
        assert!(matches!(err2, mlua::Value::Nil));
        assert_eq!(empty.len().unwrap(), 0);
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

        let (result, err): (Table, mlua::Value) =
            smol::block_on(glob.call_async::<(Table, mlua::Value)>((patterns, opts))).unwrap();
        assert!(matches!(err, mlua::Value::Nil));

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

        let (result, err): (Table, mlua::Value) =
            smol::block_on(glob.call_async::<(Table, mlua::Value)>(("*.rs", opts))).unwrap();
        assert!(matches!(err, mlua::Value::Nil));
        assert_eq!(result.len().unwrap(), 2);
    }

    #[test]
    fn glob_invalid_pattern_type_errors() {
        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let result =
            smol::block_on(glob.call_async::<Table>((mlua::Value::Integer(42), mlua::Nil)));
        assert!(result.is_err());
    }

    #[test]
    fn glob_invalid_pattern_returns_nil_err() {
        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", "/tmp").unwrap();

        let (val, err): (mlua::Value, mlua::Value) =
            smol::block_on(glob.call_async::<(mlua::Value, mlua::Value)>(("[invalid", opts)))
                .unwrap();
        assert_eq!(val, mlua::Value::Nil);
        assert!(
            matches!(&err, mlua::Value::String(s) if s.to_str().unwrap().starts_with("glob: ")),
            "should return nil, err with glob: prefix, got: {err:?}"
        );
    }

    #[test]
    fn dir_path_is_file_returns_nil_err() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("not_a_dir.txt");
        std::fs::write(&file, "i am a file").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();

        let (val, err): (mlua::Value, mlua::Value) =
            smol::block_on(dir.call_async::<(mlua::Value, mlua::Value)>(file.to_str().unwrap()))
                .unwrap();
        assert_eq!(val, mlua::Value::Nil);
        assert!(
            matches!(&err, mlua::Value::String(s) if s.to_str().unwrap().starts_with("dir: ")),
            "should return nil, err with dir: prefix, got: {err:?}"
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
        OpenOptions::new()
            .write(true)
            .open(&old_path)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        OpenOptions::new()
            .write(true)
            .open(&new_path)
            .unwrap()
            .set_modified(new_time)
            .unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        opts.set("sort", "mtime").unwrap();

        let (result, err): (Table, mlua::Value) =
            smol::block_on(glob.call_async::<(Table, mlua::Value)>(("*.rs", opts))).unwrap();
        assert!(matches!(err, mlua::Value::Nil));

        let first: String = result.get(1).unwrap();
        let second: String = result.get(2).unwrap();
        assert!(first.ends_with("new.rs"));
        assert!(second.ends_with("old.rs"));
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

        let (result, err): (Table, mlua::Value) =
            smol::block_on(glob.call_async::<(Table, mlua::Value)>(("*.rs", opts))).unwrap();
        assert!(matches!(err, mlua::Value::Nil));

        let mut paths: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            paths.push(result.get::<String>(i).unwrap());
        }
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("inner.rs"));
    }

    fn grep_call(tbl: &Table, pattern: &str, opts: Table) -> (mlua::Value, mlua::Value) {
        let grep: mlua::Function = tbl.get("grep").unwrap();
        smol::block_on(grep.call_async((pattern, opts))).unwrap()
    }

    #[test]
    fn grep_returns_matches_with_context_and_limit() {
        let tmp = TempDir::new().unwrap();
        let mut content = String::new();
        for i in 1..=20 {
            content.push_str(&format!("line_{i}\n"));
        }
        std::fs::write(tmp.path().join("data.txt"), &content).unwrap();
        std::fs::write(tmp.path().join("other.txt"), "no hits here\n").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();

        // basic match: hits data.txt, skips other.txt
        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        let (val, err) = grep_call(&tbl, "line_", opts);
        assert_eq!(err, mlua::Value::Nil);
        let result: Table = mlua::FromLua::from_lua(val, &lua).unwrap();
        assert_eq!(result.len().unwrap(), 1);
        let entry: Table = result.get(1).unwrap();
        let path = entry.get::<String>("path").unwrap();
        assert!(path.ends_with("data.txt"));
        assert!(std::path::Path::new(&path).is_absolute());
        let groups: Table = entry.get("groups").unwrap();
        assert!(groups.len().unwrap() > 0);
        let line: Table = groups
            .get::<Table>(1)
            .unwrap()
            .get::<Table>("lines")
            .unwrap()
            .get(1)
            .unwrap();
        assert!(line.get::<bool>("is_match").unwrap());
        assert!(line.get::<usize>("line_nr").unwrap() > 0);

        // context lines
        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        opts.set("context_before", 1).unwrap();
        opts.set("context_after", 1).unwrap();
        let (val, _) = grep_call(&tbl, "line_10", opts);
        let result: Table = mlua::FromLua::from_lua(val, &lua).unwrap();
        let lines: Table = result
            .get::<Table>(1)
            .unwrap()
            .get::<Table>("groups")
            .unwrap()
            .get::<Table>(1)
            .unwrap()
            .get("lines")
            .unwrap();
        assert_eq!(lines.len().unwrap(), 3);
        assert!(
            !lines
                .get::<Table>(1)
                .unwrap()
                .get::<bool>("is_match")
                .unwrap()
        );
        assert!(
            lines
                .get::<Table>(2)
                .unwrap()
                .get::<bool>("is_match")
                .unwrap()
        );
        assert!(
            !lines
                .get::<Table>(3)
                .unwrap()
                .get::<bool>("is_match")
                .unwrap()
        );

        // limit caps group count
        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        opts.set("limit", 5).unwrap();
        let (val, _) = grep_call(&tbl, "line_", opts);
        let result: Table = mlua::FromLua::from_lua(val, &lua).unwrap();
        let groups: Table = result.get::<Table>(1).unwrap().get("groups").unwrap();
        assert_eq!(groups.len().unwrap(), 5);

        // no match returns empty table, not error
        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        let (val, err) = grep_call(&tbl, "zzz_no_match", opts);
        assert_eq!(err, mlua::Value::Nil);
        let result: Table = mlua::FromLua::from_lua(val, &lua).unwrap();
        assert_eq!(result.len().unwrap(), 0);
    }

    #[test]
    fn grep_invalid_regex_returns_nil_err() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("x.txt"), "hello\n").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        let (val, err) = grep_call(&tbl, "[invalid", opts);
        assert_eq!(val, mlua::Value::Nil);
        assert!(matches!(err, mlua::Value::String(_)));
    }
}
