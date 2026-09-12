use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs::FileType;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use maki_lua_macro::{lua_fn, lua_table};
use maki_storage::paths;
use mlua::{Buffer, Lua, Result as LuaResult, Table, Value};

use crate::api::util::convert::opt_bool;
use crate::api::util::pair::{Pair, err_pair, pair, try_pair};
use crate::plugin_permissions::PluginPermissions;

const RECURSIVE_REFUSAL: &str =
    "removing this whole tree would take files with it that are out of reach";

pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    paths::expand_tilde(Path::new(path))
}

/// The one resolver every guarded call goes through, `maki.api.protected_scopes`
/// included: an accessor that resolved a spelling differently from `guarded`
/// would escalate one file and refuse another, so the user would approve a path
/// and watch the call fail anyway.
pub(crate) fn make_absolute(path: &str) -> LuaResult<PathBuf> {
    let p = expand_tilde(path);
    if p.is_absolute() {
        Ok(p)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&p))
            .map_err(|e| mlua::Error::runtime(format!("cannot resolve cwd: {e}")))
    }
}

/// Resolve `path`, refusing what only the user may have: Maki's own state, the
/// config files holding provider keys, and writes to code Maki loads on its
/// next start. `maki_storage::paths::Guard` owns the rules, the reasons and
/// the wording. Writing anywhere else, the rest of a config dir included, is
/// the permission layer's call, and it already prompts for anything outside
/// the folder the user opened.
fn guarded(path: &str, access: paths::Access) -> Result<PathBuf, String> {
    let abs = make_absolute(path).map_err(|e| e.to_string())?;
    match paths::guard().refusal_message(&abs, access, path) {
        Some(message) => Err(message),
        None => Ok(abs),
    }
}

/// The scope an approval for `path` would be recorded under, or `None` when
/// there is nothing to ask about: an ordinary path, one the guard already hands
/// over, or one only the user may ever have.
///
/// Next to `guarded` and resolving through the same function on purpose. If
/// these two disagreed about which file a spelling names, the user would be
/// prompted, approve, and watch the call fail anyway with a refusal that says
/// nothing about why. `maki.api.protected_scopes` is the only caller.
pub(crate) fn escalation_scope(path: &str, access: paths::Access) -> Option<String> {
    let abs = make_absolute(path).ok()?;
    paths::guard().override_candidate(&abs, access)?;
    paths::canonical_key(&abs)
        .to_str()
        .map(|scope| scope.to_owned())
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

/// `dir_key` is `dir` with every symlink resolved, so an entry's key costs one
/// join instead of a `realpath` per component and the listing can still ask
/// the guard about every name it finds. Same trade as the walkers in
/// `maki_agent::tools`, holding for the same reason: `read_dir` yields real
/// names and `file_type` says which of them is a link.
fn collect_dir_entries(
    base: &Path,
    dir: &Path,
    dir_key: &Path,
    depth: u32,
    max_depth: u32,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<(String, &'static str)>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let guard = paths::guard();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.strip_prefix(base).ok().and_then(|p| p.to_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let file_type = entry.file_type();
        let (type_str, is_dir) = match &file_type {
            Ok(ft) if ft.is_symlink() => match std::fs::metadata(&path) {
                Ok(meta) => (filetype_str(&meta.file_type()), meta.is_dir()),
                Err(_) => ("link", false),
            },
            Ok(ft) => (filetype_str(ft), ft.is_dir()),
            Err(_) => ("unknown", false),
        };
        // A link name says nothing about where it points, so it is the one
        // case the cheap key cannot answer.
        let key = match &file_type {
            Ok(ft) if !ft.is_symlink() => dir_key.join(entry.file_name()),
            _ => paths::canonical_key(&path),
        };
        // Naming an entry the guard would refuse to open is no use to the
        // caller, so the listing drops it, the way a search does.
        if !guard.is_unreachable_key(&key, paths::Access::Read) {
            out.push((name, type_str));
        }
        // Descend whenever anything under the entry is readable, even when the
        // entry itself is not: `Guard::may_skip_key` says why.
        if is_dir && depth < max_depth && !guard.may_skip_key(&key) && visited.insert(key.clone()) {
            collect_dir_entries(base, &path, &key, depth + 1, max_depth, visited, out);
        }
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
async fn read(_lua: Lua, path: String) -> LuaResult<Pair<String>> {
    let abs = try_pair!(guarded(&path, paths::Access::Read));
    match smol::fs::read_to_string(&abs).await {
        Ok(s) => Ok((Some(s), None)),
        Err(e) if e.kind() == ErrorKind::InvalidData => {
            Err(mlua::Error::runtime("non-utf8 content; use read_bytes"))
        }
        Err(e) => Ok(err_pair(e)),
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
async fn read_bytes(lua: Lua, path: String) -> LuaResult<Pair<Buffer>> {
    let abs = try_pair!(guarded(&path, paths::Access::Read));
    let bytes = try_pair!(smol::fs::read(&abs).await);
    Ok((Some(lua.create_buffer(bytes)?), None))
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
async fn metadata(lua: Lua, path: String) -> LuaResult<Pair<Table>> {
    let abs = try_pair!(guarded(&path, paths::Access::Read));
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
            Ok((Some(tbl), None))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok((None, None)),
        Err(e) => Ok(err_pair(e)),
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

        // No error slot in the signature, so a refused start answers like a
        // search that found nothing.
        let Ok(mut dir) = guarded(start.to_str().unwrap_or_default(), paths::Access::Read) else {
            return Ok(None);
        };

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
async fn dir(lua: Lua, path: String, opts: Option<Table>) -> LuaResult<Pair<Table>> {
    let abs = try_pair!(guarded(&path, paths::Access::Read));
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
        let key = paths::canonical_key(&abs);
        collect_dir_entries(&abs, &abs, &key, 1, max_depth, &mut visited, &mut out);
        Ok(out)
    })
    .await;

    let entries = try_pair!(result);
    let tbl = lua.create_table()?;
    for (i, (name, typ)) in entries.iter().enumerate() {
        let entry = lua.create_table()?;
        entry.set(1, name.as_str())?;
        entry.set(2, *typ)?;
        tbl.set(i + 1, entry)?;
    }
    Ok((Some(tbl), None))
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
async fn write(_lua: Lua, path: String, content: String) -> LuaResult<Pair<bool>> {
    let abs = try_pair!(guarded(&path, paths::Access::Write));
    Ok(pair(smol::fs::write(&abs, content).await.map(|()| true)))
}

/// Append {content} to the file at {path}, creating it (but not its parent
/// directory) if it does not exist.
///
/// @param path string Destination file path. `~/` is expanded.
/// @param content string Text to append.
/// @return (true?, string?) `true` on success, or nil plus an error message.
/// @example
/// local ok, err = maki.fs.append("out.log", "line\n")
/// if err then print("append failed: " .. err) end
#[lua_fn(guard = FsWrite)]
async fn append(_lua: Lua, path: String, content: String) -> LuaResult<Pair<bool>> {
    let abs = try_pair!(guarded(&path, paths::Access::Write));
    // `smol::fs::File` writes through a background task and answers before
    // the bytes reach the file, so a plain `unblock` keeps append ordered.
    let result = smol::unblock(move || {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&abs)
            .and_then(|mut f| f.write_all(content.as_bytes()))
    })
    .await;
    Ok(pair(result.map(|()| true)))
}

/// Atomically replace {path} with {content}. The parent directory must exist.
/// Readers observe either the old file or the complete new file.
/// Existing file permissions are preserved. On Unix, new files use mode 0600.
///
/// @param path string Destination file path. `~/` is expanded.
/// @param content string Text to write.
/// @return (true?, string?) `true` on success, or nil plus an error message.
/// @example
/// local ok, err = maki.fs.atomic_write("state.json", encoded)
/// if err then print("atomic write failed: " .. err) end
#[lua_fn(guard = FsWrite)]
async fn atomic_write(_lua: Lua, path: String, content: String) -> LuaResult<Pair<bool>> {
    let abs = try_pair!(guarded(&path, paths::Access::Write));
    let result = smol::unblock(move || maki_storage::atomic_write(&abs, content.as_bytes())).await;
    Ok(pair(result.map(|()| true)))
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
async fn rm(_lua: Lua, path: String, opts: Option<Table>) -> LuaResult<Pair<bool>> {
    let abs = try_pair!(guarded(&path, paths::Access::Write));
    let recursive = opts
        .as_ref()
        .and_then(|t| opt_bool(t, "recursive"))
        .unwrap_or(false);
    let force = opts
        .as_ref()
        .and_then(|t| opt_bool(t, "force"))
        .unwrap_or(false);
    // `guarded` only looked at the path it was handed, and a recursive
    // removal reaches everything below it without naming any of it.
    if recursive && paths::guard().contains_unwritable(&abs) {
        return Ok(err_pair(format!(
            "{}: {path}: {RECURSIVE_REFUSAL}",
            paths::REFUSED
        )));
    }
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
    Ok(pair(result.map(|()| true)))
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
async fn mkdir(_lua: Lua, path: String, opts: Option<Table>) -> LuaResult<Pair<bool>> {
    let abs = try_pair!(guarded(&path, paths::Access::Write));
    let parents = opts
        .as_ref()
        .and_then(|t| opt_bool(t, "parents"))
        .unwrap_or(false);
    let result = if parents {
        smol::fs::create_dir_all(&abs).await
    } else {
        smol::fs::create_dir(&abs).await
    };
    Ok(pair(result.map(|()| true)))
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
async fn glob(lua: Lua, pattern: Value, opts: Option<Table>) -> LuaResult<Pair<Table>> {
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
        .and_then(|t| opt_bool(t, "gitignore"))
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

    let paths = try_pair!(result.map_err(|e| format!("glob: {e}")));
    let tbl = lua.create_table()?;
    for (i, path) in paths.iter().enumerate() {
        tbl.set(i + 1, path.as_str())?;
    }
    Ok((Some(tbl), None))
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
async fn grep(lua: Lua, pattern: String, opts: Option<Table>) -> LuaResult<Pair<Table>> {
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

    let (base, entries) = try_pair!(result);
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
    Ok((Some(arr), None))
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
        dir(perms), write(perms), append(perms), atomic_write(perms), rm(perms), mkdir(perms),
        glob(perms), grep(perms),
    ]
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::sync::OnceLock;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::plugin_permissions::PluginPermissions;
    use mlua::Lua;
    use tempfile::TempDir;
    use test_case::test_case;

    const FIRST_CONTENT: &str = "first";
    const REPLACEMENT_CONTENT: &str = "replacement";
    const FS_WRITE_PERMISSION: &str = "fs_write";
    const STATE_FILE: &str = "kept.json";
    const INIT_LUA: &str = "init.lua";
    const PERMISSIONS_TOML: &str = "permissions.toml";
    const ENV_FILE: &str = ".env";
    const PROVIDERS_TOML: &str = "providers.toml";
    const MCP_TOML: &str = "mcp.toml";
    const PROVIDER_SCRIPT: &str = "providers/mycorp";
    const LUA_MODULE: &str = "lua/browser.lua";
    const PROJECT_INIT_LUA: &str = ".maki/init.lua";
    const PROJECT_LUA_MODULE: &str = ".maki/lua/helper.lua";
    const PROJECT_PERMISSIONS_TOML: &str = ".maki/permissions.toml";
    const CONFIG_TOML: &str = "config.toml";
    const SUBDIR: &str = "sub";
    const NOTE_FILE: &str = "note.md";
    const SECRET_NOTE: &str = "ledger.md";
    const NOTES_GLOB: &str = "**/*.md";
    const ROUND_TRIP_DIR: &str = "round_trip";
    const PRUNED_SEARCH_DIR: &str = "pruned_search";
    const REMOVED_DIR: &str = "removed";
    const STATE_ROLE: &str = "state";
    const DATA_ROLE: &str = "data";
    const CONFIG_ROLE: &str = "config";
    const REFUSAL_EXPECTED: &str = "a protected path must be refused";
    const ESCALATION_MATCHES_THE_CALL: &str =
        "an escalation is offered exactly where the call is refused";
    const SCOPE_IS_CANONICAL: &str =
        "the answer is recorded against the file the call opens, not the spelling that reached it";
    const NO_PROMPT_FOR_ORDINARY_PATHS: &str =
        "an ordinary path must not put a permission prompt in front of a read";
    const SEALED_OFFERS_NOTHING: &str = "no prompt hands over what only the user may have";
    const PACKAGE_MODULE: &str = "site/lua/browser.lua";
    const LEXICAL_ESCAPE: &str = "a lexical spelling must not decide the rule";
    const GIT_MARKER: &str = ".git";
    const ANY_PATTERN: &str = "x";
    const GUARD_ALREADY_RESOLVED: &str =
        "something asked for the process rule set before this layout was installed";

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
    fn atomic_write_creates_and_replaces_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("state.json");
        let lua = Lua::new();
        let table = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let atomic_write: mlua::Function = table.get("atomic_write").unwrap();

        for content in [FIRST_CONTENT, REPLACEMENT_CONTENT] {
            let (ok, err): (Value, Value) =
                smol::block_on(atomic_write.call_async((file.to_str().unwrap(), content))).unwrap();
            assert_eq!(ok, Value::Boolean(true));
            assert_eq!(err, Value::Nil);
            assert_eq!(std::fs::read_to_string(&file).unwrap(), content);
        }
    }

    #[test]
    fn atomic_write_returns_error_when_parent_is_missing() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("missing/state.json");
        let lua = Lua::new();
        let table = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let atomic_write: mlua::Function = table.get("atomic_write").unwrap();

        let (ok, err): (Value, Value) =
            smol::block_on(atomic_write.call_async((file.to_str().unwrap(), FIRST_CONTENT)))
                .unwrap();

        assert_eq!(ok, Value::Nil);
        assert!(matches!(err, Value::String(_)));
        assert!(!file.exists());
    }

    #[test]
    fn atomic_write_requires_fs_write_permission() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("state.json");
        let lua = Lua::new();
        let table = create_fs_table(&lua, &PluginPermissions::denied()).unwrap();
        let atomic_write: mlua::Function = table.get("atomic_write").unwrap();

        let error = smol::block_on(
            atomic_write.call_async::<(Value, Value)>((file.to_str().unwrap(), FIRST_CONTENT)),
        )
        .unwrap_err();

        assert!(error.to_string().contains(FS_WRITE_PERMISSION));
        assert!(!file.exists());
    }

    #[test]
    fn append_creates_then_appends_to_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("out.log");
        let lua = Lua::new();
        let table = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let append: mlua::Function = table.get("append").unwrap();

        let (ok, err): (Value, Value) =
            smol::block_on(append.call_async((file.to_str().unwrap(), FIRST_CONTENT))).unwrap();
        assert_eq!(ok, Value::Boolean(true));
        assert_eq!(err, Value::Nil);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), FIRST_CONTENT);

        let (ok, err): (Value, Value) =
            smol::block_on(append.call_async((file.to_str().unwrap(), REPLACEMENT_CONTENT)))
                .unwrap();
        assert_eq!(ok, Value::Boolean(true));
        assert_eq!(err, Value::Nil);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{FIRST_CONTENT}{REPLACEMENT_CONTENT}")
        );
    }

    #[test]
    fn append_returns_error_when_parent_is_missing() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("missing/out.log");
        let lua = Lua::new();
        let table = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let append: mlua::Function = table.get("append").unwrap();

        let (ok, err): (Value, Value) =
            smol::block_on(append.call_async((file.to_str().unwrap(), FIRST_CONTENT))).unwrap();

        assert_eq!(ok, Value::Nil);
        assert!(matches!(err, Value::String(_)));
        assert!(!file.exists());
    }

    #[test]
    fn append_requires_fs_write_permission() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("out.log");
        let lua = Lua::new();
        let table = create_fs_table(&lua, &PluginPermissions::denied()).unwrap();
        let append: mlua::Function = table.get("append").unwrap();

        let error = smol::block_on(
            append.call_async::<(Value, Value)>((file.to_str().unwrap(), FIRST_CONTENT)),
        )
        .unwrap_err();

        assert!(error.to_string().contains(FS_WRITE_PERMISSION));
        assert!(!file.exists());
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

    /// The fixture ignores through `.ignore` so the walker needs no git repo,
    /// and hides a directory rather than a file because the glob patterns turn
    /// into whitelist overrides that outrank a file-level ignore rule.
    #[test_case(None, 0 ; "omitted_key_keeps_the_true_default")]
    #[test_case(Some(false), 1 ; "false_includes_ignored_files")]
    fn glob_gitignore_option(gitignore: Option<bool>, expected_hits: i64) {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".ignore"), "sub/\n").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/ignored.log"), "").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        if let Some(gitignore) = gitignore {
            opts.set("gitignore", gitignore).unwrap();
        }

        let (result, err): (Table, mlua::Value) =
            smol::block_on(glob.call_async::<(Table, mlua::Value)>(("**/*.log", opts))).unwrap();
        assert!(matches!(err, mlua::Value::Nil));
        assert_eq!(result.len().unwrap(), expected_hits);
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

    /// A layout these tests own, standing in for the real state, data and
    /// config dirs.
    ///
    /// Installed process-wide rather than held beside the real one, because a
    /// `maki.fs` call and the walkers it hands work to live in different crates
    /// and have to answer alike. With a second `Guard` here, a search root came
    /// back refused in this file while the walk underneath was still asking
    /// about the developer's real home directory.
    ///
    /// nextest gives each test its own process, so the install always lands.
    /// Asserted rather than assumed: a tempdir is in nobody's state dir, so a
    /// guard that quietly stayed the real one would leave everything below
    /// passing for the wrong reason.
    static FIXTURE: OnceLock<TempDir> = OnceLock::new();

    fn fixture() -> &'static TempDir {
        FIXTURE.get_or_init(|| {
            let root = TempDir::new().unwrap();
            let dir = |role: &str| {
                let path = root.path().join(role);
                std::fs::create_dir_all(&path).unwrap();
                path
            };
            let guard = paths::Guard::for_layout(&paths::Layout {
                state: Some(&dir(STATE_ROLE)),
                data: Some(&dir(DATA_ROLE)),
                config_dirs: &[dir(CONFIG_ROLE)],
                cache: None,
                logs: None,
                home: None,
            });
            assert!(paths::install_guard(guard), "{GUARD_ALREADY_RESOLVED}");
            root
        })
    }

    fn fixture_dir(role: &str) -> PathBuf {
        fixture().path().join(role)
    }

    /// One of the state-dir subtrees a feature depends on, read out of the rules
    /// so a rename cannot leave a case asking about a closed directory.
    fn open_state_subtree() -> &'static str {
        paths::open_state_subtrees()
            .next()
            .expect("the state dir keeps at least one open subtree")
    }

    fn state_path(rel: &str) -> String {
        path_string(&fixture_dir(STATE_ROLE).join(rel))
    }

    fn config_path(rel: &str) -> String {
        path_string(&fixture_dir(CONFIG_ROLE).join(rel))
    }

    fn path_string(path: &Path) -> String {
        path.to_str().unwrap().to_owned()
    }

    fn fs_table(lua: &Lua) -> Table {
        create_fs_table(lua, &PluginPermissions::trusted()).unwrap()
    }

    fn call<R: mlua::FromLuaMulti>(
        tbl: &Table,
        func_name: &str,
        args: impl mlua::IntoLuaMulti,
    ) -> R {
        let f: mlua::Function = tbl.get(func_name).unwrap();
        smol::block_on(f.call_async(args)).unwrap()
    }

    /// Asks for the reason too, because "some error came back" passes just as
    /// well when the path was refused for the wrong rule or a write failed on
    /// its own. The refusal is looked for inside the message rather than at
    /// the front of it, because a search names the tool first and the rule
    /// that refused its root is reported from the walk underneath.
    fn assert_refused(
        tbl: &Table,
        func_name: &str,
        args: impl mlua::IntoLuaMulti,
        expected: paths::Refusal,
    ) {
        let (value, err): (mlua::Value, Option<String>) = call(tbl, func_name, args);
        assert_eq!(
            value,
            mlua::Value::Nil,
            "{func_name} must not return a value"
        );
        let message = err.expect(REFUSAL_EXPECTED);
        assert!(
            message.contains(paths::REFUSED) && message.contains(expected.as_str()),
            "{func_name} refused with the wrong error: {message}"
        );
    }

    /// The other half of `assert_refused`: a call that must go through. Asks
    /// for the error slot rather than the value, so a refusal is reported as
    /// the refusal it is instead of a failed conversion three frames away.
    fn assert_allowed(tbl: &Table, func_name: &str, args: impl mlua::IntoLuaMulti) {
        let (_, err): (mlua::Value, Option<String>) = call(tbl, func_name, args);
        assert_eq!(err, None, "{func_name} must be allowed");
    }

    /// `<state>/<first open subtree>/{name}`: where the memory plugin keeps
    /// its notes. A directory per test, so the tests cannot see each other's
    /// files if they ever share a process.
    fn note_dir(name: &str) -> PathBuf {
        fixture_dir(STATE_ROLE)
            .join(open_state_subtree())
            .join(name)
    }

    fn entry_names(listed: &Table) -> Vec<String> {
        listed
            .sequence_values::<Table>()
            .flatten()
            .filter_map(|entry| entry.get::<String>(1).ok())
            .collect()
    }

    /// The files a search came back with, for either shape `maki.fs` answers
    /// in: `glob` a list of paths, `grep` a list of entries keyed by `path`.
    fn found_paths(found: &Table) -> Vec<String> {
        found
            .sequence_values::<Value>()
            .flatten()
            .filter_map(|value| match value {
                Value::String(s) => s.to_str().ok().map(|s| s.to_owned()),
                Value::Table(entry) => entry.get::<String>("path").ok(),
                _ => None,
            })
            .collect()
    }

    /// A symlink to the state dir, so a path spelled through it only lands on a
    /// rule once the link is resolved.
    #[cfg(unix)]
    fn state_link(tmp: &TempDir) -> PathBuf {
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(fixture_dir(STATE_ROLE), &link).unwrap();
        link
    }

    #[test_case("read")]
    #[test_case("read_bytes")]
    #[test_case("metadata")]
    #[test_case("dir")]
    #[test_case("mkdir")]
    #[test_case("rm")]
    fn refuses_a_path_in_makis_state(func_name: &str) {
        let lua = Lua::new();
        assert_refused(
            &fs_table(&lua),
            func_name,
            state_path(STATE_FILE),
            paths::Refusal::OwnState,
        );
    }

    #[test_case("write")]
    #[test_case("append")]
    #[test_case("atomic_write")]
    fn refuses_a_write_into_makis_state(func_name: &str) {
        let lua = Lua::new();
        assert_refused(
            &fs_table(&lua),
            func_name,
            (state_path(STATE_FILE), FIRST_CONTENT),
            paths::Refusal::OwnState,
        );
    }

    /// The boundary stops at Maki's state on purpose, so everything a user
    /// keeps in a config dir stays writable, the provider scripts included.
    /// Writing those is still a question, asked by the permission layer, which
    /// prompts for any path outside the folder the user opened. A project's
    /// `.maki` is folder trust's business, and an untrusted folder's `init.lua`
    /// never runs at all.
    #[test_case("write")]
    #[test_case("append")]
    #[test_case("atomic_write")]
    fn leaves_config_dirs_and_project_maki_dirs_alone(func_name: &str) {
        let project = TempDir::new().unwrap();
        let lua = Lua::new();
        let tbl = fs_table(&lua);

        for path in [
            config_path(INIT_LUA),
            config_path(LUA_MODULE),
            config_path(PROVIDER_SCRIPT),
            path_string(&project.path().join(PROJECT_INIT_LUA)),
            path_string(&project.path().join(PROJECT_LUA_MODULE)),
            path_string(&project.path().join(PROJECT_PERMISSIONS_TOML)),
        ] {
            std::fs::create_dir_all(Path::new(&path).parent().unwrap()).unwrap();
            let (_, err): (mlua::Value, Option<String>) =
                call(&tbl, func_name, (path.clone(), FIRST_CONTENT));
            assert_eq!(err, None, "{path} must be writable");
        }
    }

    /// The file that writes down what Maki may do, including which of Maki's own
    /// files an approval opened. Readable, so the agent can explain the rules it
    /// is running under; never writable, because a policy the agent can edit is
    /// no policy. Maki's own writer is unaffected: the guard gates `maki.fs`.
    #[test]
    fn reads_the_permission_policy_and_never_writes_it() {
        let path = config_path(PERMISSIONS_TOML);
        std::fs::create_dir_all(Path::new(&path).parent().unwrap()).unwrap();
        std::fs::write(&path, FIRST_CONTENT).unwrap();
        let lua = Lua::new();
        let tbl = fs_table(&lua);

        assert_eq!(call::<String>(&tbl, "read", path.clone()), FIRST_CONTENT);
        assert_refused(
            &tbl,
            "write",
            (path, FIRST_CONTENT),
            paths::Refusal::PermissionPolicy,
        );
    }

    /// The config files that hold keys in plaintext. Nothing in Lua reads
    /// them, so refusing them costs no feature, and the refusal names the
    /// reason so the agent can tell the user what to edit themselves instead
    /// of retrying with another spelling.
    #[test_case("read"; "reading_one")]
    #[test_case("metadata"; "looking_at_one")]
    fn refuses_a_config_file_holding_keys(func_name: &str) {
        let lua = Lua::new();
        let tbl = fs_table(&lua);

        for rel in [ENV_FILE, PROVIDERS_TOML, MCP_TOML] {
            let (value, err): (mlua::Value, Option<String>) =
                call(&tbl, func_name, config_path(rel));
            assert_eq!(value, mlua::Value::Nil, "{rel} must not come back");
            assert!(
                err.expect(REFUSAL_EXPECTED)
                    .contains(paths::Refusal::Credentials.as_str()),
                "{rel} must be refused for holding credentials"
            );
        }
    }

    /// A plugin `require`s Lua modules out of an installed package and reading
    /// one is how anyone reviews it, so a checkout stays readable through the
    /// closed data dir around it. Never writable: see `Reach::ReadOnly`. The
    /// rest of the data dir is closed, and nothing else lives there today,
    /// which is the point: whatever lands there next is covered already.
    #[test]
    fn a_package_checkout_is_readable_and_the_data_dir_is_not() {
        let package = fixture_dir(DATA_ROLE).join(paths::SITE_DIR).join(NOTE_FILE);
        std::fs::create_dir_all(package.parent().unwrap()).unwrap();
        std::fs::write(&package, FIRST_CONTENT).unwrap();
        let lua = Lua::new();
        let tbl = fs_table(&lua);

        assert_eq!(
            call::<String>(&tbl, "read", path_string(&package)),
            FIRST_CONTENT
        );
        assert_refused(
            &tbl,
            "write",
            (path_string(&package), FIRST_CONTENT),
            paths::Refusal::PackageCode,
        );
        assert_refused(
            &tbl,
            "read",
            path_string(&fixture_dir(DATA_ROLE).join(NOTE_FILE)),
            paths::Refusal::OwnState,
        );
    }

    /// A listing that stopped at the closed state dir would hide the notes the
    /// memory plugin keeps in an open subtree of it, so the same note was
    /// listed or not depending on where the listing started. What it must
    /// still not do is name the files Maki keeps for itself.
    #[test]
    fn dir_walks_past_makis_state_into_the_open_subtrees() {
        let state = fixture_dir(STATE_ROLE);
        let open = state.join(open_state_subtree());
        std::fs::create_dir_all(&open).unwrap();
        std::fs::write(open.join(NOTE_FILE), FIRST_CONTENT).unwrap();
        std::fs::write(state.join(STATE_FILE), FIRST_CONTENT).unwrap();

        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("depth", 3).unwrap();
        let listed: Table = call(
            &fs_table(&lua),
            "dir",
            (path_string(fixture().path()), opts),
        );
        let names = entry_names(&listed);

        assert!(
            names.iter().any(|name| name.ends_with(NOTE_FILE)),
            "an open subtree must survive the listing of its closed parent: {names:?}"
        );
        assert!(
            !names.iter().any(|name| name.ends_with(STATE_FILE)),
            "the listing named a file Maki keeps for itself: {names:?}"
        );
    }

    /// Every subtree on the open list backs a feature that stops working the
    /// day it drops off, and stops quietly.
    #[test]
    fn open_state_subtrees_stay_reachable() {
        let lua = Lua::new();
        let tbl = fs_table(&lua);

        for name in paths::open_state_subtrees() {
            let (_, err): (mlua::Value, Option<String>) = call(&tbl, "metadata", state_path(name));
            assert!(
                !err.unwrap_or_default().contains(paths::REFUSED),
                "{name} must stay reachable"
            );
        }
    }

    #[test]
    fn refuses_makis_state_as_search_root() {
        let lua = Lua::new();
        let tbl = fs_table(&lua);

        let opts = lua.create_table().unwrap();
        opts.set("path", path_string(&fixture_dir(STATE_ROLE)))
            .unwrap();
        assert_refused(
            &tbl,
            "glob",
            (ANY_PATTERN, opts.clone()),
            paths::Refusal::OwnState,
        );
        assert_refused(&tbl, "grep", (ANY_PATTERN, opts), paths::Refusal::OwnState);
    }

    /// `guarded` has to resolve the path rather than match its text, or a link
    /// in a directory the agent writes is a way around every rule here.
    #[cfg(unix)]
    #[test]
    fn refuses_a_spelled_path_into_makis_state() {
        let tmp = TempDir::new().unwrap();
        let spelled = state_link(&tmp).join(STATE_FILE);
        assert!(
            !paths::normalize_path(&spelled).starts_with(fixture_dir(STATE_ROLE)),
            "{LEXICAL_ESCAPE}"
        );

        let lua = Lua::new();
        assert_refused(
            &fs_table(&lua),
            "read",
            path_string(&spelled),
            paths::Refusal::OwnState,
        );
    }

    /// `rm -r` never names the files it destroys, so checking the path it was
    /// handed says nothing: the parent of the state dir is an ordinary path.
    #[test]
    fn refuses_a_recursive_remove_that_reaches_makis_files() {
        let lua = Lua::new();
        let tbl = fs_table(&lua);
        let opts = lua.create_table().unwrap();
        opts.set("recursive", true).unwrap();
        opts.set("force", true).unwrap();

        let (value, err): (mlua::Value, Option<String>) =
            call(&tbl, "rm", (path_string(fixture().path()), opts));
        assert_eq!(value, mlua::Value::Nil, "rm must not report success");
        assert!(
            err.expect(REFUSAL_EXPECTED).contains(RECURSIVE_REFUSAL),
            "rm refused for the wrong reason"
        );
        assert!(
            fixture_dir(STATE_ROLE).is_dir(),
            "the refusal must leave the tree alone"
        );
    }

    /// `root` has no error slot, so a refused start answers like a search that
    /// found nothing. The marker beside the link is what an unguarded walk up
    /// out of the state dir would have returned.
    #[cfg(unix)]
    #[test]
    fn root_finds_nothing_from_inside_makis_state() {
        let tmp = TempDir::new().unwrap();
        let start = state_link(&tmp).join(STATE_FILE);
        std::fs::write(tmp.path().join(GIT_MARKER), "").unwrap();

        let lua = Lua::new();
        let found: Option<String> =
            call(&fs_table(&lua), "root", (path_string(&start), GIT_MARKER));
        assert_eq!(found, None);
    }

    /// The memory plugin's whole round trip: make the note's directory, write
    /// the note, read it back. `open_state_subtrees_stay_reachable` only asks
    /// for metadata, so a rule that turned the open subtrees read-only would
    /// pass every other test here while memory notes and plan mode quietly
    /// stopped saving anything.
    #[test_case("write")]
    #[test_case("append")]
    #[test_case("atomic_write")]
    fn writes_and_reads_back_a_note_in_an_open_state_subtree(func_name: &str) {
        let dir = note_dir(ROUND_TRIP_DIR);
        let note = path_string(&dir.join(format!("{func_name}_{NOTE_FILE}")));

        let lua = Lua::new();
        let tbl = fs_table(&lua);
        let opts = lua.create_table().unwrap();
        opts.set("parents", true).unwrap();

        assert_allowed(&tbl, "mkdir", (path_string(&dir), opts));
        assert_allowed(&tbl, func_name, (note.clone(), FIRST_CONTENT));
        assert_eq!(call::<String>(&tbl, "read", note), FIRST_CONTENT);
    }

    /// A search started above Maki's state has to walk into it for the notes
    /// and still come back without the files beside them, which is
    /// `may_skip_key` and the per-entry drop working together rather than
    /// either alone. Both files are notes with the same contents, so the
    /// pattern that finds one would find the other.
    ///
    /// Also the one test where the two crates have to agree on a path that is
    /// *reachable*: `glob` and `grep` hand the walk to `maki_agent::tools`,
    /// which resolves the root against the same rules.
    /// `refuses_makis_state_as_search_root` only proves they agree on a
    /// refusal, which a guard refusing everything would satisfy too.
    #[test_case("glob", NOTES_GLOB ; "glob")]
    #[test_case("grep", FIRST_CONTENT ; "grep")]
    fn a_search_across_makis_state_keeps_the_open_notes_and_drops_the_rest(
        func_name: &str,
        pattern: &str,
    ) {
        let dir = note_dir(PRUNED_SEARCH_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(NOTE_FILE), FIRST_CONTENT).unwrap();
        std::fs::write(fixture_dir(STATE_ROLE).join(SECRET_NOTE), FIRST_CONTENT).unwrap();
        let note = path_string(&dir.join(NOTE_FILE));

        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("path", path_string(fixture().path())).unwrap();
        let found: Table = call(&fs_table(&lua), func_name, (pattern, opts));
        let paths = found_paths(&found);

        assert!(
            paths.iter().any(|path| path.ends_with(&note)),
            "{func_name} must reach an open subtree through the closed dir around it: {paths:?}"
        );
        assert!(
            !paths.iter().any(|path| path.ends_with(SECRET_NOTE)),
            "{func_name} returned a file Maki keeps for itself: {paths:?}"
        );
    }

    /// Deleting a note is a recursive remove of the directory holding it, so
    /// the extra check `rm` does for `recursive` decides whether memory can
    /// forget anything. `contains_unwritable` counts no open rule on purpose,
    /// and a stricter version refuses this while nothing else here notices,
    /// since `refuses_a_recursive_remove_that_reaches_makis_files` wants a
    /// refusal anyway.
    #[test]
    fn removes_a_note_tree_inside_an_open_state_subtree() {
        let dir = note_dir(REMOVED_DIR);
        let nested = dir.join(SUBDIR);
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(NOTE_FILE), FIRST_CONTENT).unwrap();

        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("recursive", true).unwrap();

        assert_allowed(&fs_table(&lua), "rm", (path_string(&dir), opts));
        assert!(!dir.exists(), "the note tree must be gone");
    }

    /// A config dir is where dropping entries per name matters: the files
    /// holding keys sit right beside the user's own config, so the listing has
    /// to leave three names out of the middle of one directory and keep the
    /// rest. `dir_walks_past_makis_state_into_the_open_subtrees` only ever
    /// drops a whole branch.
    #[test]
    fn dir_over_a_config_dir_names_the_users_files_and_not_the_keys() {
        let config = fixture_dir(CONFIG_ROLE);
        for name in [ENV_FILE, PROVIDERS_TOML, MCP_TOML, CONFIG_TOML, INIT_LUA] {
            std::fs::write(config.join(name), FIRST_CONTENT).unwrap();
        }

        let lua = Lua::new();
        let listed: Table = call(&fs_table(&lua), "dir", path_string(&config));
        let names = entry_names(&listed);

        for own in [CONFIG_TOML, INIT_LUA] {
            assert!(
                names.iter().any(|name| name == own),
                "the user's own {own} must be listed: {names:?}"
            );
        }
        for credentials in [ENV_FILE, PROVIDERS_TOML, MCP_TOML] {
            assert!(
                !names.iter().any(|name| name == credentials),
                "the listing named {credentials}, which holds keys: {names:?}"
            );
        }
    }

    /// The approved-but-refused class, which is the one failure nobody can debug
    /// from the message: if `protected_scopes` and `maki.fs` disagreed about
    /// which file a spelling names, the user would be prompted, approve, and the
    /// call would still come back refused.
    ///
    /// So the property is asked of every spelling of one file: an escalation is
    /// offered exactly where the call is refused, and the scope it offers is the
    /// canonical path, which is what the answer gets recorded against.
    #[test_case(|state, _| state.join(STATE_FILE); "absolute")]
    #[test_case(|state, _| state.join(SUBDIR).join("..").join(STATE_FILE); "back_out_of_a_subdir")]
    #[test_case(|state, _| state.join(".").join(STATE_FILE); "with_a_dot_component")]
    #[cfg_attr(unix, test_case(|_, link| link.join(STATE_FILE); "through_a_symlink"))]
    fn an_escalation_names_the_file_the_call_would_open(spell: fn(&Path, &Path) -> PathBuf) {
        let tmp = TempDir::new().unwrap();
        let state = fixture_dir(STATE_ROLE);
        std::fs::create_dir_all(state.join(SUBDIR)).unwrap();
        #[cfg(unix)]
        let link = state_link(&tmp);
        #[cfg(not(unix))]
        let link = tmp.path().to_path_buf();
        let spelled = path_string(&spell(&state, &link));

        let scope = escalation_scope(&spelled, paths::Access::Read);

        assert!(
            guarded(&spelled, paths::Access::Read).is_err(),
            "{spelled}: {ESCALATION_MATCHES_THE_CALL}"
        );
        assert_eq!(
            scope.as_deref(),
            paths::canonical_key(&state.join(STATE_FILE)).to_str(),
            "{spelled}: {SCOPE_IS_CANONICAL}"
        );
    }

    /// The regression guard for "the agent suddenly prompts on every file read".
    /// An ordinary path answers with nothing, so a read tool returns no scope and
    /// the permission layer is never reached.
    #[test_case(|tmp, _| tmp.join(NOTE_FILE); "a_file_of_the_users_own")]
    #[test_case(|_, state| state.join(open_state_subtree()).join(NOTE_FILE); "an_open_state_subtree")]
    fn an_ordinary_path_asks_nothing(spell: fn(&Path, &Path) -> PathBuf) {
        let tmp = TempDir::new().unwrap();
        let spelled = path_string(&spell(tmp.path(), &fixture_dir(STATE_ROLE)));

        assert_eq!(
            escalation_scope(&spelled, paths::Access::Read),
            None,
            "{spelled}: {NO_PROMPT_FOR_ORDINARY_PATHS}"
        );
        assert!(guarded(&spelled, paths::Access::Read).is_ok());
    }

    /// No prompt can hand these over, so the accessor offers no scope for them
    /// and the refusal the call already gives is the whole answer.
    #[test_case(STATE_ROLE, paths::APPROVALS_FILE, paths::Access::Read; "the_approval_store")]
    #[test_case(CONFIG_ROLE, ENV_FILE, paths::Access::Read; "an_env_file")]
    #[test_case(CONFIG_ROLE, PERMISSIONS_TOML, paths::Access::Write; "the_permission_policy")]
    #[test_case(DATA_ROLE, PACKAGE_MODULE, paths::Access::Write; "package_code")]
    fn a_sealed_path_offers_no_escalation(role: &str, rel: &str, access: paths::Access) {
        let spelled = path_string(&fixture_dir(role).join(rel));

        assert_eq!(
            escalation_scope(&spelled, access),
            None,
            "{spelled}: {SEALED_OFFERS_NOTHING}"
        );
        assert!(guarded(&spelled, access).is_err(), "{spelled}");
    }
}
