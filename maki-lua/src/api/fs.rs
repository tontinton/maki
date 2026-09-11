use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::{File, FileType};
use std::io::{Error as IoError, ErrorKind, Read, Result as IoResult};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use maki_agent::{FileQuery, FileReader, Ranked};
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Buffer, Lua, Result as LuaResult, Table, Value};

use crate::api::util::convert::opt_bool;
use crate::api::util::pair::{Pair, err_pair, pair, try_pair};
use crate::loader::EventHandle;
use crate::plugin_permissions::PluginPermissions;
use crate::runtime::LUA_MEMORY_LIMIT;

// Luau allows strings and buffers up to 1 GiB, but the VM budget is the binding
// limit: a read the VM cannot hold dies with a Lua memory error instead.
const MAX_READ_BYTES: u64 = LUA_MEMORY_LIMIT as u64;

pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    maki_storage::paths::expand_tilde(Path::new(path))
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

async fn read_file(path: PathBuf, max_bytes: u64) -> IoResult<Vec<u8>> {
    smol::unblock(move || {
        let too_large = || {
            IoError::new(
                ErrorKind::FileTooLarge,
                format!("file exceeds the {max_bytes}-byte read limit"),
            )
        };
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        if size > max_bytes {
            return Err(too_large());
        }

        // Files can grow, and some streams report a size of zero.
        let mut bytes = Vec::with_capacity(size as usize);
        file.take(max_bytes + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            return Err(too_large());
        }
        Ok(bytes)
    })
    .await
}

/// Read the file at {path} as a UTF-8 string.
/// Files larger than 512 MiB return nil plus an error message.
/// If the file contains bytes that are not valid UTF-8, this function throws.
/// Use `read_bytes` for binary files.
///
/// @param path string Absolute or relative file path. `~/` is expanded to the home directory.
/// @param opts table? `{ offset = integer, len = integer }` window to read. A negative
///   `offset` counts back from the end of the file, so `{ offset = -1024 }` reads the
///   last 1024 bytes, and `len` caps how many bytes are read from `offset`. A window
///   that splits a multibyte character replaces the broken sequence. Omit `opts` to
///   read the whole file.
/// @return (string?, string?) File contents, or nil plus an error message.
/// @example
/// local text, err = maki.fs.read("config.toml")
/// if err then
///   maki.log.warn("could not read config: " .. err)
///   return
/// end
/// @example
/// local tail = maki.fs.read("server.log", { offset = -4096 })
#[lua_fn(guard = FsRead)]
async fn read(_lua: Lua, path: String, opts: Option<Table>) -> LuaResult<Pair<String>> {
    let abs = make_absolute(&path)?;
    let window = match opts.as_ref() {
        Some(t) => {
            let offset: Option<i64> = t.get("offset")?;
            let len: Option<u64> = t.get("len")?;
            Some((offset.unwrap_or(0), len.unwrap_or(u64::MAX)))
        }
        None => None,
    };
    let Some((offset, len)) = window else {
        let bytes = try_pair!(read_file(abs, MAX_READ_BYTES).await);
        return match String::from_utf8(bytes) {
            Ok(s) => Ok((Some(s), None)),
            Err(_) => Err(mlua::Error::runtime("non-utf8 content; use read_bytes")),
        };
    };
    let too_large = || {
        IoError::new(
            ErrorKind::FileTooLarge,
            format!("file exceeds the {MAX_READ_BYTES}-byte read limit"),
        )
    };
    let text = smol::unblock(move || -> std::io::Result<String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(&abs)?;
        let start = if offset < 0 {
            file.metadata()?.len().saturating_sub(offset.unsigned_abs())
        } else {
            offset as u64
        };
        file.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::new();
        file.take(len.min(MAX_READ_BYTES + 1)).read_to_end(&mut buf)?;
        if buf.len() as u64 > MAX_READ_BYTES {
            return Err(too_large());
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    })
    .await;
    match text {
        Ok(s) => Ok((Some(s), None)),
        Err(e) => Ok(err_pair(e)),
    }
}

/// Read the entire file at {path} as raw bytes, returned as a Luau buffer.
/// Files larger than 512 MiB return nil plus an error message.
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
    let abs = make_absolute(&path)?;
    let bytes = try_pair!(read_file(abs, MAX_READ_BYTES).await);
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
async fn dir(lua: Lua, path: String, opts: Option<Table>) -> LuaResult<Pair<Table>> {
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
    let abs = make_absolute(&path)?;
    let result = smol::fs::write(&abs, content).await;
    Ok(pair(touched(abs, result).await.map(|()| true)))
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
    let abs = make_absolute(&path)?;
    let appended = abs.clone();
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
    Ok(pair(touched(appended, result).await.map(|()| true)))
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
    let abs = make_absolute(&path)?;
    let written = abs.clone();
    let result = smol::unblock(move || maki_storage::atomic_write(&abs, content.as_bytes())).await;
    Ok(pair(touched(written, result).await.map(|()| true)))
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
    let abs = make_absolute(&path)?;
    let recursive = opts
        .as_ref()
        .and_then(|t| opt_bool(t, "recursive"))
        .unwrap_or(false);
    let force = opts
        .as_ref()
        .and_then(|t| opt_bool(t, "force"))
        .unwrap_or(false);
    let removed = abs.clone();
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
    Ok(pair(touched(removed, result).await.map(|()| true)))
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
    let abs = make_absolute(&path)?;
    let parents = opts
        .as_ref()
        .and_then(|t| opt_bool(t, "parents"))
        .unwrap_or(false);
    let result = if parents {
        smol::fs::create_dir_all(&abs).await
    } else {
        smol::fs::create_dir(&abs).await
    };
    Ok(pair(touched(abs, result).await.map(|()| true)))
}

/// The path {path} really names, for the prefix match the file index picks
/// its roots by.
///
/// [`make_absolute`] only joins the cwd, so `../sibling/f` keeps the `..`
/// that leaves it spelled as if it were still under the root it climbed out
/// of, and a root reached through a symlink is spelled nothing like the
/// canonical one the index is keyed on. The path itself is left as it is: a
/// deleted path is exactly when this matters most and it is no longer there
/// to resolve, and a removed symlink names the tree it sat in rather than
/// the one it pointed at. So the nearest ancestor that does exist is
/// resolved and the rest of the spelling is put back on the end of it, which
/// is what an `rm -r` of a whole directory leaves to work with.
///
/// Components rather than [`Path::file_name`], which has no answer for a
/// path whose last component is `..`: `rm("<dir>/..")` climbed nowhere at
/// all and was handed to the index with every symlink it started with.
fn resolved(path: &Path) -> PathBuf {
    let mut climbed = Vec::new();
    let mut at = path;
    while let (Some(parent), Some(tail)) = (at.parent(), at.components().next_back()) {
        climbed.push(tail);
        if let Ok(parent) = parent.canonicalize() {
            return climbed.iter().rev().fold(parent, descend);
        }
        at = parent;
    }
    path.to_path_buf()
}

/// Puts one component of the unresolved tail back on {base}, which is
/// canonical. Leaving a `..` in place would spell a path that prefix-matches
/// no root the index keeps.
///
/// `..` is the directory above whatever the path so far really names, so a
/// tail holding a component that does exist is resolved before climbing out
/// of it: `link/..` with `link -> /other/x` is `/other`, which is where the
/// kernel goes, and popping the spelling instead would name the tree the link
/// sits in and leave the one that moved stale. A component that is no longer
/// there has nothing to resolve and is climbed lexically, which is as much as
/// a spelling alone can say.
///
/// `.` never arrives here: `std::path::Components` keeps one only at the
/// front of a relative path, and every path this is reassembling came from
/// [`make_absolute`].
fn descend(mut base: PathBuf, component: &Component<'_>) -> PathBuf {
    match component {
        Component::ParentDir => {
            if let Ok(real) = base.canonicalize() {
                base = real;
            }
            base.pop();
        }
        named => base.push(named),
    }
    base
}

/// Passes {result} through, telling the file index that {path} came or went
/// when it says the call worked.
///
/// The write and edit tools are the reason the tree moves while maki runs,
/// and the index is otherwise the tree as it was up to twenty seconds ago:
/// the picker would not offer a file the agent just wrote, and would offer
/// one it just deleted. A failed call changed nothing, so it says nothing.
///
/// Resolving the path is a realpath per ancestor and one more per `..` left
/// in the tail, and this runs on the one executor every plugin shares, so it
/// waits for a blocking thread the way the call it follows did.
async fn touched<E>(path: PathBuf, result: Result<(), E>) -> Result<(), E> {
    if result.is_ok() {
        smol::unblock(move || maki_agent::invalidate_for(&resolved(&path))).await;
    }
    result
}

/// Find files matching one or more glob patterns, walked fresh on every call.
///
/// Reads any path the plugin is allowed to read and keeps nothing afterwards.
/// `maki.fs.fuzzy_files` ranks a walk the host caches instead, which is
/// cheaper per keystroke but only covers the current working directory.
///
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

/// Ranked paths asked for more than this at once stop being a completion and
/// start being a copy of the repo.
const MAX_FILE_RESULTS: usize = 500;
const DEFAULT_FILE_RESULTS: usize = 20;
/// Roots one plugin keeps indexed. Each one is a walk of a tree and a path
/// list the host keeps while the plugin is reading it, so a loop over the
/// subdirectories of the cwd would otherwise leave one of each behind per
/// iteration. Completing paths is one root, and a few spare cover a plugin
/// that follows the user into another directory; the one it asked for
/// longest ago is let go rather than refused, because a plugin that followed
/// the user through five directories would otherwise be stuck on the first
/// four for the rest of the session.
const MAX_PLUGIN_ROOTS: usize = 4;
const SUPERSEDED_ERR: &str = "a newer maki.fs.fuzzy_files call took over";
const OUTSIDE_CWD_ERR: &str =
    "maki.fs.fuzzy_files only indexes the current working directory and below";
/// The event a walk landing fires, so a plugin ranking files can wait for the
/// list instead of polling it.
const WALK_EVENT: &str = "FileIndexReady";

/// The roots one plugin is reading, least recently asked for first.
type PluginRoots = Vec<(PathBuf, FileReader)>;

thread_local! {
    /// The cancel flag of each plugin's in-flight `fuzzy_files` call. A plugin
    /// filtering a list as the user types only ever wants the newest answer,
    /// so asking again gives up on the last one instead of letting the two
    /// race to paint.
    static FILE_QUERIES: RefCell<HashMap<Arc<str>, Arc<AtomicBool>>> =
        RefCell::new(HashMap::new());
    /// The roots each plugin is reading, least recently asked for first. The
    /// reader is what keeps the walk alive: the built-in picker closing
    /// cancels the walk it was reading, and a plugin holding one of these is
    /// the reason that cancel is not the end of it.
    static PLUGIN_ROOTS: RefCell<HashMap<Arc<str>, PluginRoots>> = RefCell::new(HashMap::new());
}

/// Cancels {plugin}'s previous query and returns the flag for the new one.
fn supersede(plugin: &Arc<str>) -> Arc<AtomicBool> {
    let token = Arc::new(AtomicBool::new(false));
    FILE_QUERIES.with_borrow_mut(|live| {
        if let Some(previous) = live.insert(Arc::clone(plugin), Arc::clone(&token)) {
            previous.store(true, Ordering::Relaxed);
        }
    });
    token
}

/// Forgets {token} unless a later call already replaced it, so the map holds
/// one flag per plugin with a query in flight rather than one per call.
fn retire(plugin: &Arc<str>, token: &Arc<AtomicBool>) {
    FILE_QUERIES.with_borrow_mut(|live| {
        if live
            .get(plugin)
            .is_some_and(|live| Arc::ptr_eq(live, token))
        {
            live.remove(plugin);
        }
    });
}

/// The root a `fuzzy_files` call may index, confined to {base}.
///
/// The host retains the full path list behind this answer for the session, so
/// a plugin naming `/`, or `..` its way out of the project, would pin a list
/// of every path on the machine in a process-wide index. Canonicalising first
/// also makes every spelling of one directory reach one index instead of one
/// walk per spelling, and what it returns is what the index is keyed on, so
/// the directory that was cleared is the directory that is walked.
fn search_root(path: Option<&str>, base: &Path) -> Result<PathBuf, String> {
    let Some(path) = path else {
        return Ok(base.to_path_buf());
    };
    let resolved = maki_agent::tools::resolve_search_path(Some(path))?;
    let root = Path::new(&resolved)
        .canonicalize()
        .map_err(|e| format!("{path}: {e}"))?;
    if !root.starts_with(base) {
        return Err(OUTSIDE_CWD_ERR.to_owned());
    }
    Ok(root)
}

fn cwd_root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd error: {e}"))?;
    Ok(cwd.canonicalize().unwrap_or(cwd))
}

/// Records that {plugin} is reading {root} and keeps it to
/// `MAX_PLUGIN_ROOTS`: the reader it asked for longest ago is dropped, which
/// gives up its claim on that walk.
fn remember_root(plugin: &Arc<str>, root: PathBuf, reader: FileReader) {
    PLUGIN_ROOTS.with_borrow_mut(|indexed| {
        let roots = indexed.entry(Arc::clone(plugin)).or_default();
        roots.retain(|(indexed, _)| *indexed != root);
        roots.push((root, reader));
        if roots.len() > MAX_PLUGIN_ROOTS {
            roots.remove(0);
        }
    });
}

/// Drops everything {plugin} was holding of the file index.
///
/// Both maps are keyed by plugin name and nothing else ever takes a name out
/// of them, so an unloaded or reloaded plugin would go on holding readers
/// that keep its walks alive: the host could never evict those roots, and
/// each one pins a path list the size of the tree for the rest of the
/// session. The in-flight query is cancelled on the way out, because the Lua
/// side that was going to publish its answer is gone.
pub(crate) fn clear_plugin_files(plugin: &str) {
    FILE_QUERIES.with_borrow_mut(|live| {
        if let Some(token) = live.remove(plugin) {
            token.store(true, Ordering::Relaxed);
        }
    });
    PLUGIN_ROOTS.with_borrow_mut(|indexed| indexed.remove(plugin));
}

/// Turns every walk that lands into a `FileIndexReady` autocmd, so a plugin
/// ranking files waits for the list instead of polling for it. Installed once
/// per plugin host, because the walks outlive any one of them.
///
/// A root with no UTF-8 spelling is left out rather than sent as a lossy one:
/// the replacement characters make a name that is not a path, and two roots
/// that differ only where the replacement lands come through as one, so a
/// plugin comparing the event against its own root would match the wrong
/// tree. Absent, the comparison fails and the plugin asks again.
pub(crate) fn publish_walks(handle: EventHandle) {
    maki_agent::on_walk_end(move |walk| {
        handle.fire_autocmd(
            WALK_EVENT,
            serde_json::json!({
                "root": walk.root.to_str(),
                "files": walk.files,
                "crashed": walk.crashed,
                "truncated": walk.truncated,
            }),
        );
    });
}

/// What one `fuzzy_files` call brings back off the blocking thread: the root
/// it settled on, the reader that keeps that walk alive, and the ranking
/// itself, which is absent once a newer call has taken over.
type RankedRoot = Result<(PathBuf, FileReader, Option<Ranked>), String>;

/// Rank the files and directories under {opts.path} against {opts.query} and
/// return the best ones, best first, with the state of the walk behind them.
///
/// The host walks each root once and shares that walk with the built-in file
/// picker (`Ctrl+S`), so a call ranks an existing list instead of walking the
/// tree, and both rank alike. Use `maki.fs.glob` for patterns, a path outside
/// the cwd, or a tree read fresh right now. Only `limit` items ever cross into
/// Lua whatever the size of the repo, paths come back relative to the root
/// with a trailing separator on the directories, and `.gitignore` and `.git`
/// are respected. `highlights` cost a second matcher pass, so ask for them
/// only to draw matches.
///
/// `complete` is false while a walk is filling the list, so an empty `items`
/// means "not found yet" and asking again is worth it. maki walks a bounded
/// number of trees at once, so a first call can also answer before the walk it
/// asked for has started. `crashed` and `truncated` are the two ways a
/// complete list is still not the whole tree: a walker that died partway
/// through it, and a tree bigger than the host's ceiling. Listen for
/// `"FileIndexReady"` with `maki.api.create_autocmd` to be told when a walk
/// lands rather than polling for it.
///
/// The tools that write, move and delete files mark the tree they touched, so
/// a call a moment after an edit re-walks and a file the agent just wrote is
/// findable. A file a `bash` command creates or deletes is not: a shell
/// command cannot say what it touched, and maki does not try to guess. That
/// leaves the staleness window as the only guarantee, and it is this: a walk
/// is redone the first time anything asks for the index more than twenty
/// seconds after the last one landed, so a path may be missing from the list,
/// or offered after it is gone, for up to twenty seconds.
///
/// A second call from the same plugin cancels the one in flight, which answers
/// with nil plus an error. While the user types that is expected, so treat the
/// error as a stale answer. A plugin reading more than four roots keeps the
/// four it asked for most recently: the walk behind an older one is let go and
/// asking for it again walks it again.
///
/// @param opts table? Options:
///   `query` (string) what the user typed. Empty returns the first `limit` paths in walk order.
///   `limit` (integer) how many items to return, at most 500. Default 20.
///   `path` (string) the root to search, the cwd or below it. Default is the current working directory.
///   `highlights` (boolean) also return where the query matched each path. Default false.
/// @return (table?, string?) `{ root = string, complete = boolean, crashed = boolean, truncated = boolean, items = { { path = string, highlights = integer[][]? } } }`, or nil plus an error message. `root` is the resolved absolute directory the paths are relative to, spelled the way `"FileIndexReady"` spells it, so an event can be matched to the call that caused it. It is absent for a directory with no UTF-8 spelling, as it is on the event, so a plugin that gets no `root` treats every event as a reason to ask again rather than matching the wrong tree. `items` is a 1-based array, best first. `highlights` is only present when asked for, and holds `{ from, to }` byte ranges of `path`, ascending, 1-based and inclusive, so `path:sub(from, to)` is the matched text.
/// @example
/// local res, err = maki.fs.fuzzy_files({ query = "src/mai", limit = 20, highlights = true })
/// if err then return end -- a newer call took over, this answer is stale
/// for _, item in ipairs(res.items) do
///   local r = item.highlights[1]
///   print(item.path, r and item.path:sub(r[1], r[2]))
/// end
/// if not res.complete then print("still scanning, ask again") end
#[lua_fn(guard = FsRead)]
async fn fuzzy_files(
    lua: Lua,
    #[ctx] plugin: Arc<str>,
    opts: Option<Table>,
) -> LuaResult<Pair<Table>> {
    let query = opts
        .as_ref()
        .and_then(|o| o.get::<String>("query").ok())
        .unwrap_or_default();
    let limit = opts
        .as_ref()
        .and_then(|o| o.get::<usize>("limit").ok())
        .unwrap_or(DEFAULT_FILE_RESULTS)
        .min(MAX_FILE_RESULTS);
    let highlights = opts
        .as_ref()
        .and_then(|o| opt_bool(o, "highlights"))
        .unwrap_or(false);
    let path = opts.as_ref().and_then(|o| o.get::<String>("path").ok());

    let cancel = supersede(&plugin);
    let token = Arc::clone(&cancel);
    // Resolving the root stats the filesystem twice, so it waits for the
    // blocking thread with the ranking: on a network filesystem those two
    // syscalls per keystroke stall every other plugin on the Lua executor.
    let answer = smol::unblock(move || -> RankedRoot {
        // `search_root` resolved this root to confine it, so the index is
        // keyed on that very path: resolving it again would be a second
        // answer, and a component swapped for a symlink between the two would
        // put the walk outside the directory the check cleared.
        let root = search_root(path.as_deref(), &cwd_root()?)?;
        // The reader comes first so the walk cannot be cancelled out from
        // under the ranking by another reader letting go mid-query.
        let reader = maki_agent::resolved_index(root.clone()).reader();
        let found = reader.query(
            &FileQuery {
                query: &query,
                limit,
                highlights,
            },
            &token,
        );
        Ok((root, reader, found))
    })
    .await;
    retire(&plugin, &cancel);
    let (root, reader, found) = try_pair!(answer);

    // Two calls from one plugin are two blocking tasks that resume in
    // completion order, so the overtaken one can come back last. `query` only
    // looks at the flag as it goes, so the last word on whether this answer
    // may be published has to be here, after the await.
    let Some(found) = found.filter(|_| !cancel.load(Ordering::Relaxed)) else {
        return Ok(err_pair(SUPERSEDED_ERR.to_owned()));
    };
    // Only now, because the root list is a least-recently-asked-for one: a
    // keystroke the user has already moved past re-inserting its root as the
    // newest can evict the root the plugin is actually completing against.
    let name = root.to_str().map(str::to_owned);
    remember_root(&plugin, root, reader);

    let items = lua.create_table_with_capacity(found.items.len(), 0)?;
    for item in &found.items {
        let entry = lua.create_table_with_capacity(0, 1 + usize::from(highlights))?;
        entry.set("path", item.path.as_str())?;
        if highlights {
            let ranges = lua.create_table_with_capacity(item.highlights.len(), 0)?;
            for (from, to) in &item.highlights {
                ranges.push(lua.create_sequence_from([*from, *to])?)?;
            }
            entry.set("highlights", ranges)?;
        }
        items.push(entry)?;
    }
    let tbl = lua.create_table_with_capacity(0, 5)?;
    // The event that says a walk landed names the canonical root, and Lua has
    // no way of its own to canonicalise, so without this a plugin that passed
    // a symlinked or relative spelling cannot tell whether the event is about
    // its own query. A root with no UTF-8 spelling is left out here as well,
    // so the two are either the same string or no match at all.
    tbl.set("root", name)?;
    tbl.set("complete", found.complete)?;
    tbl.set("crashed", found.crashed)?;
    tbl.set("truncated", found.truncated)?;
    tbl.set("items", items)?;
    Ok((Some(tbl), None))
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
    "maki.fs" => pub(crate) fn create_fs_table(perms: &PluginPermissions, plugin: Arc<str>), DOCS [
        read(perms), read_bytes(perms), metadata(perms), dirname, basename,
        joinpath, normalize, abspath, parents, root(perms), relpath, ext,
        dir(perms), write(perms), append(perms), atomic_write(perms), rm(perms), mkdir(perms),
        glob(perms), grep(perms), fuzzy_files(perms, plugin),
    ]
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::time::{Duration, Instant, SystemTime};

    use super::*;
    use crate::plugin_permissions::PluginPermissions;
    use mlua::Lua;
    use tempfile::TempDir;
    use test_case::test_case;

    const FIRST_CONTENT: &str = "first";
    const REPLACEMENT_CONTENT: &str = "replacement";
    const FS_WRITE_PERMISSION: &str = "fs_write";
    #[cfg(unix)]
    const READ_LIMIT_ERROR: &str = "file exceeds the 536870912-byte read limit";
    const NON_UTF8_ERROR: &str = "non-utf8 content; use read_bytes";
    const TEST_READ_LIMIT: u64 = 4;
    const TEST_PLUGIN: &str = "test";

    #[test]
    fn read_file_ok() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        std::fs::write(&file, "world").unwrap();

        let lua = Lua::new();
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
        let read: mlua::Function = tbl.get("read").unwrap();
        let result: String = smol::block_on(read.call_async(file.to_str().unwrap())).unwrap();
        assert_eq!(result, "world");
    }

    #[test]
    fn read_missing_returns_nil_err() {
        let lua = Lua::new();
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();

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

    // Sparse files keep these oversized-file tests cheap on Unix filesystems.
    #[cfg(unix)]
    #[test_case("read"; "text")]
    #[test_case("read_bytes"; "binary")]
    fn oversized_read_returns_nil_err(func_name: &str) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oversized");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_READ_BYTES + 1).unwrap();

        let lua = Lua::new();
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
        let f: mlua::Function = tbl.get(func_name).unwrap();
        let (value, err): (Value, Option<String>) =
            smol::block_on(f.call_async(path.to_str().unwrap())).unwrap();
        assert_eq!(value, Value::Nil);
        assert_eq!(err.as_deref(), Some(READ_LIMIT_ERROR));
    }

    #[test_case(b""; "empty")]
    #[test_case(b"abc"; "below_limit")]
    #[test_case(b"abcd"; "at_limit")]
    fn bounded_read_accepts_contents_within_limit(contents: &[u8]) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bounded");
        std::fs::write(&path, contents).unwrap();

        let bytes = smol::block_on(read_file(path, TEST_READ_LIMIT)).unwrap();
        assert_eq!(bytes, contents);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_read_limits_stream_with_zero_reported_size() {
        let path = PathBuf::from("/dev/zero");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);

        let err = smol::block_on(read_file(path, TEST_READ_LIMIT)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::FileTooLarge);
    }

    #[test_case(b""; "empty")]
    #[test_case(b"\x00\xff\x80"; "non_utf8")]
    fn read_bytes_preserves_binary_contents(contents: &[u8]) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("binary");
        std::fs::write(&path, contents).unwrap();

        let lua = Lua::new();
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
        let f: mlua::Function = tbl.get("read_bytes").unwrap();
        let (buffer, err): (Buffer, Option<String>) =
            smol::block_on(f.call_async(path.to_str().unwrap())).unwrap();
        assert_eq!(buffer.to_vec(), contents);
        assert_eq!(err, None);
    }

    #[test]
    fn read_non_utf8_still_throws() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("binary");
        std::fs::write(&path, b"\xff").unwrap();

        let lua = Lua::new();
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
        let f: mlua::Function = tbl.get("read").unwrap();
        let err = smol::block_on(f.call_async::<Value>(path.to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains(NON_UTF8_ERROR));
    }

    const WINDOW_LOG_CONTENT: &str = "head-tail-IGNORED\nmiddle\nthe-end";
    const WINDOW_LOG_TAIL: &str = "the-end";
    const WINDOW_LOG_WINDOW: &str = "tail";

    #[test_case(-7, None, WINDOW_LOG_TAIL; "negative_offset_reads_from_end")]
    #[test_case(5, Some(4), WINDOW_LOG_WINDOW; "offset_and_len_read_a_window")]
    #[test_case(-10_000, None, WINDOW_LOG_CONTENT; "window_larger_than_file_reads_all")]
    fn read_window(offset: i64, len: Option<u64>, expected: &str) {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("log.txt");
        std::fs::write(&file, WINDOW_LOG_CONTENT).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let read: mlua::Function = tbl.get("read").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("offset", offset).unwrap();
        if let Some(len) = len {
            opts.set("len", len).unwrap();
        }

        let (text, err): (String, mlua::Value) =
            smol::block_on(read.call_async((file.to_str().unwrap(), opts))).unwrap();
        assert!(matches!(err, mlua::Value::Nil), "windowed read succeeds");
        assert_eq!(text, expected);
    }

    #[test]
    fn dir_lists_entries() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let lua = Lua::new();
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let table =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let table =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let table =
            create_fs_table(&lua, &PluginPermissions::denied(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let table =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let table =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let table =
            create_fs_table(&lua, &PluginPermissions::denied(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let result =
            smol::block_on(glob.call_async::<Table>((mlua::Value::Integer(42), mlua::Nil)));
        assert!(result.is_err());
    }

    #[test]
    fn glob_invalid_pattern_returns_nil_err() {
        let lua = Lua::new();
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();
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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();

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
        let tbl =
            create_fs_table(&lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        let (val, err) = grep_call(&tbl, "[invalid", opts);
        assert_eq!(val, mlua::Value::Nil);
        assert!(matches!(err, mlua::Value::String(_)));
    }

    const FILE_COUNT: usize = 40;
    const SMALL_LIMIT: usize = 3;
    const WALK_TIMEOUT: Duration = Duration::from_secs(10);
    const MAIN_FILE: &str = "main.rs";
    const MAIN_QUERY: &str = "mainrs";
    const PARITY_QUERY: &str = "f1rs";
    const NEVER_WALKED: &str = "the walk never finished";
    const ESCAPE_PATH: &str = "..";
    const FS_ROOT_PATH: &str = "/";
    const PICKER_MAIN: &str = "src/main.rs";
    /// No `a` after either of its `m`s, so `main` is the one query below that
    /// cannot reach it.
    const PICKER_README: &str = "docs/readme.md";
    const HIGHLIGHT_QUERY: &str = "main";
    const HIGHLIGHTED_ROW: &str = "src/[main].rs";
    /// Where `main` lands in `src/main.rs`, counting bytes from one and
    /// closing on the last of them.
    const HIGHLIGHT_RANGE: [u32; 2] = [5, 8];
    const SETTLED_WALK: &str = "the walk was waited out before the query";
    /// Written after the first walk of the tree it lands in, so it is only
    /// found if the write told the index the tree had moved.
    const NEW_FILE: &str = "new_thing.rs";
    /// Its own plugin name, so the roots it spends are not roots another test
    /// on this thread has to share.
    const ROOT_HUNGRY_PLUGIN: &str = "root-hungry";
    /// Its own plugin name again, because the point of the test is that
    /// nothing of it is left behind.
    const UNLOADED_PLUGIN: &str = "unloaded";
    #[cfg(unix)]
    const TOUCHED_FILE: &str = "touched.rs";
    #[cfg(unix)]
    const REAL_DIR: &str = "real";
    #[cfg(unix)]
    const LINK_DIR: &str = "link";
    #[cfg(unix)]
    const NESTED_DIR: &str = "nested";
    /// Unlinked before the path naming it is resolved, which is what leaves
    /// the tail below it to be put back by hand.
    #[cfg(unix)]
    const GONE_DIR: &str = "gone";
    /// One call has to give Lua everything a picker draws: the ranked rows,
    /// the characters to highlight, and whether the walk is still running.
    const PICKER_LUA: &str = r#"
        local fs, opts = ...
        local res, err = fs.fuzzy_files(opts)
        if err then error(err) end
        local rows = {}
        for _, item in ipairs(res.items) do
            local row, at = "", 1
            -- ranges are 1-based and inclusive, exactly what sub takes
            for _, r in ipairs(item.highlights) do
                row = row .. item.path:sub(at, r[1] - 1) .. "[" .. item.path:sub(r[1], r[2]) .. "]"
                at = r[2] + 1
            end
            rows[#rows + 1] = row .. item.path:sub(at)
        end
        return { scanning = not res.complete, rows = rows }
    "#;

    /// A tree big enough that a limit has to do something. It lives under the
    /// cwd because `maki.fs.fuzzy_files` refuses anything else.
    fn file_tree(base: &Path) -> TempDir {
        let tmp = TempDir::new_in(base).unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        for i in 0..FILE_COUNT {
            std::fs::write(tmp.path().join("src").join(format!("f{i}.rs")), "").unwrap();
        }
        std::fs::write(tmp.path().join(MAIN_FILE), "").unwrap();
        tmp
    }

    /// The walk runs on its own thread, so anything comparing two answers has
    /// to wait for it or they are answers about different corpora. Asking
    /// again on the way is how a walk the host had no room to start yet still
    /// gets started, which is what a reader does too.
    fn walked(root: &Path) {
        let index = maki_agent::file_index(root);
        let deadline = Instant::now() + WALK_TIMEOUT;
        while !index.corpus().complete {
            assert!(Instant::now() < deadline, "{NEVER_WALKED}");
            index.refresh();
            std::thread::yield_now();
        }
    }

    fn files_opts(lua: &Lua, root: &Path, query: &str, limit: usize, highlights: bool) -> Table {
        walked(root);
        let opts = lua.create_table().unwrap();
        opts.set("path", root.to_str().unwrap()).unwrap();
        opts.set("query", query).unwrap();
        opts.set("limit", limit).unwrap();
        opts.set("highlights", highlights).unwrap();
        opts
    }

    fn files_answer(tbl: &Table, opts: Table) -> Table {
        let f: mlua::Function = tbl.get("fuzzy_files").unwrap();
        let (found, err): (Option<Table>, Option<String>) =
            smol::block_on(f.call_async(opts)).unwrap();
        assert_eq!(err, None, "the query must not have been superseded");
        found.unwrap()
    }

    fn files_call(lua: &Lua, tbl: &Table, root: &Path, query: &str, limit: usize) -> Table {
        files_answer(tbl, files_opts(lua, root, query, limit, false))
    }

    fn items(found: &Table) -> Vec<Table> {
        found
            .get::<Table>("items")
            .unwrap()
            .sequence_values::<Table>()
            .map(Result::unwrap)
            .collect()
    }

    fn found_paths(found: &Table) -> Vec<String> {
        items(found)
            .iter()
            .map(|item| item.get::<String>("path").unwrap())
            .collect()
    }

    fn fs_table(lua: &Lua) -> Table {
        create_fs_table(lua, &PluginPermissions::trusted(), Arc::from(TEST_PLUGIN)).unwrap()
    }

    /// A tree with one path the highlight query reaches and one it cannot, so
    /// the rows a query draws are the ranking as well as the highlighting.
    fn small_tree(base: &Path) -> TempDir {
        let tmp = TempDir::new_in(base).unwrap();
        for path in [PICKER_MAIN, PICKER_README] {
            let file = tmp.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "").unwrap();
        }
        tmp
    }

    struct Painted {
        scanning: bool,
        rows: Vec<String>,
    }

    fn picker_call(lua: &Lua, tbl: &Table, opts: Table) -> Painted {
        let drawn: Table = smol::block_on(lua.load(PICKER_LUA).call_async((tbl, opts))).unwrap();
        Painted {
            scanning: drawn.get("scanning").unwrap(),
            rows: drawn
                .get::<Table>("rows")
                .unwrap()
                .sequence_values::<String>()
                .map(Result::unwrap)
                .collect(),
        }
    }

    #[test]
    fn files_ranks_the_closest_path_first() {
        let base = cwd_root().unwrap();
        let tmp = file_tree(&base);
        let lua = Lua::new();
        let got = files_call(&lua, &fs_table(&lua), tmp.path(), MAIN_QUERY, SMALL_LIMIT);
        assert_eq!(
            found_paths(&got).first().map(String::as_str),
            Some(MAIN_FILE)
        );
    }

    /// The ranking is the file picker's, so the same corpus and the same
    /// config have to produce the same order or a plugin picker and `Ctrl+S`
    /// disagree about the same query.
    #[test]
    fn files_ranks_the_way_the_built_in_picker_does() {
        let base = cwd_root().unwrap();
        let tmp = file_tree(&base);
        let lua = Lua::new();
        let got = files_call(&lua, &fs_table(&lua), tmp.path(), PARITY_QUERY, SMALL_LIMIT);
        let want: Vec<String> = maki_agent::file_index(tmp.path())
            .query(
                &FileQuery {
                    query: PARITY_QUERY,
                    limit: SMALL_LIMIT,
                    highlights: false,
                },
                &AtomicBool::new(false),
            )
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.path)
            .collect();
        assert_eq!(found_paths(&got), want);
    }

    /// Nothing proportional to the size of the tree is allowed to cross into
    /// Lua, however big the tree or however greedy the caller.
    #[test]
    fn files_never_hands_lua_more_than_it_asked_for() {
        let base = cwd_root().unwrap();
        let tmp = file_tree(&base);
        let lua = Lua::new();
        let tbl = fs_table(&lua);
        assert_eq!(
            found_paths(&files_call(&lua, &tbl, tmp.path(), "", SMALL_LIMIT)).len(),
            SMALL_LIMIT
        );
        assert!(
            found_paths(&files_call(&lua, &tbl, tmp.path(), "", usize::MAX)).len()
                <= MAX_FILE_RESULTS,
            "an absurd limit is capped rather than honoured"
        );
    }

    /// The answer is retained in a process-wide index, so a root outside the
    /// project would pin a list of paths nobody can drop.
    #[test_case(FS_ROOT_PATH ; "the_whole_filesystem")]
    #[test_case(ESCAPE_PATH  ; "a_walk_out_of_the_project")]
    fn files_refuses_a_root_outside_the_project(path: &str) {
        let base = cwd_root().unwrap();
        assert_eq!(search_root(Some(path), &base).unwrap_err(), OUTSIDE_CWD_ERR);
    }

    #[test]
    fn files_defaults_to_the_project_root() {
        let base = cwd_root().unwrap();
        assert_eq!(search_root(None, &base).unwrap(), base);
    }

    /// Every spelling of one directory has to reach one index, or a plugin
    /// gets a walk and a retained list per spelling it can think of.
    #[test]
    fn files_resolves_every_spelling_of_a_root_to_one_path() {
        let base = cwd_root().unwrap();
        let tmp = TempDir::new_in(&base).unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let spellings = [
            root.display().to_string(),
            format!("{}/", root.display()),
            format!("{}/.", root.display()),
        ];
        for spelling in &spellings {
            assert_eq!(search_root(Some(spelling), &base).unwrap(), root);
        }
    }

    /// A keystroke that has been overtaken gets an error rather than an
    /// answer, so the plugin drawing the list cannot paint the older one.
    #[test]
    fn a_superseded_file_query_gives_up_on_the_older_answer() {
        let plugin: Arc<str> = Arc::from(TEST_PLUGIN);
        let first = supersede(&plugin);
        let second = supersede(&plugin);
        assert!(
            first.load(Ordering::Relaxed),
            "the older call was cancelled"
        );
        assert!(!second.load(Ordering::Relaxed), "the newer one was not");
        retire(&plugin, &second);
        assert!(FILE_QUERIES.with_borrow(|live| !live.contains_key(&plugin)));
    }

    /// The flag can land while the blocking task is already on its way back,
    /// so an answer that was built before the cancel still must not be
    /// published, and nothing it learned on the way may be kept either.
    #[test]
    fn a_query_superseded_mid_flight_publishes_nothing() {
        let base = cwd_root().unwrap();
        let tmp = file_tree(&base);
        walked(tmp.path());

        let lua = Lua::new();
        let tbl = fs_table(&lua);
        let plugin: Arc<str> = Arc::from(TEST_PLUGIN);
        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        let f: mlua::Function = tbl.get("fuzzy_files").unwrap();

        let (found, err): (Option<Table>, Option<String>) = smol::block_on(async {
            let mut call = Box::pin(f.call_async::<(Option<Table>, Option<String>)>(opts));
            // One poll runs the call up to its first await, which is where it
            // has taken its flag and handed the ranking to a blocking task.
            // Superseding it now is the flag landing while the answer it is
            // about to return is already built.
            assert!(
                futures_lite::future::poll_once(&mut call).await.is_none(),
                "the call is supposed to be waiting on its ranking here"
            );
            let overtaking = supersede(&plugin);
            let out = call.await;
            retire(&plugin, &overtaking);
            out
        })
        .unwrap();
        assert!(found.is_none());
        assert_eq!(err.as_deref(), Some(SUPERSEDED_ERR));
        // The roots are a least-recently-asked-for list, so an overtaken
        // keystroke re-inserting its own as the newest can drop the walk
        // behind the directory the plugin is actually completing against.
        assert!(
            plugin_roots(&plugin).is_empty(),
            "an overtaken call leaves the roots the plugin holds alone"
        );
    }

    /// Highlights are a second matcher pass over the items that made the cut,
    /// so a plugin that only lists paths must not pay for them.
    #[test]
    fn files_omits_highlights_until_they_are_asked_for() {
        let base = cwd_root().unwrap();
        let tmp = small_tree(&base);
        let lua = Lua::new();
        let tbl = fs_table(&lua);

        let bare = files_call(&lua, &tbl, tmp.path(), HIGHLIGHT_QUERY, SMALL_LIMIT);
        assert!(
            items(&bare)[0].get::<Value>("highlights").unwrap().is_nil(),
            "nothing asked for them"
        );

        let opts = files_opts(&lua, tmp.path(), HIGHLIGHT_QUERY, SMALL_LIMIT, true);
        let asked = files_answer(&tbl, opts);
        let ranges: Vec<Vec<u32>> = items(&asked)[0]
            .get::<Table>("highlights")
            .unwrap()
            .sequence_values::<Table>()
            .map(|range| {
                range
                    .unwrap()
                    .sequence_values()
                    .map(Result::unwrap)
                    .collect()
            })
            .collect();
        assert_eq!(
            ranges,
            [HIGHLIGHT_RANGE.to_vec()],
            "one range, 1-based and inclusive, over the whole match"
        );
    }

    /// One call has to be enough to draw a row: the ranking puts the right
    /// path first, and the ranges say which of it to mark.
    #[test]
    fn a_plugin_picker_can_draw_a_ranked_highlighted_row_from_one_call() {
        let base = cwd_root().unwrap();
        let tmp = small_tree(&base);
        let lua = Lua::new();
        let tbl = fs_table(&lua);
        let opts = files_opts(&lua, tmp.path(), HIGHLIGHT_QUERY, SMALL_LIMIT, true);

        let painted = picker_call(&lua, &tbl, opts);
        assert_eq!(
            painted.rows,
            [HIGHLIGHTED_ROW],
            "the ranked rows, with the matched characters marked"
        );
        assert!(!painted.scanning, "{SETTLED_WALK}");
    }

    /// One plugin cancelling its own query must not cancel another plugin's.
    #[test]
    fn plugins_do_not_supersede_each_other() {
        let mine: Arc<str> = Arc::from("mine");
        let yours: Arc<str> = Arc::from("yours");
        let first = supersede(&mine);
        let second = supersede(&yours);
        assert!(!first.load(Ordering::Relaxed));
        retire(&mine, &first);
        retire(&yours, &second);
    }

    /// A reader over a root nothing walks, so a test can fill a plugin's
    /// budget without a walker per fake root.
    fn reader_for(root: &Path) -> FileReader {
        maki_agent::FileIndex::detached(root).reader()
    }

    fn plugin_roots(plugin: &Arc<str>) -> Vec<PathBuf> {
        PLUGIN_ROOTS.with_borrow(|indexed| {
            indexed
                .get(plugin)
                .map(|roots| roots.iter().map(|(root, _)| root.clone()).collect())
                .unwrap_or_default()
        })
    }

    /// Root confinement says where a plugin may index, not how much, and a
    /// loop over the subdirectories of the cwd is inside the project. Each
    /// root is a walk and a path list maki holds while the plugin reads it,
    /// so only a handful are held at once.
    #[test]
    fn a_plugin_only_ever_holds_a_handful_of_roots() {
        let plugin: Arc<str> = Arc::from(ROOT_HUNGRY_PLUGIN);
        let roots: Vec<PathBuf> = (0..=MAX_PLUGIN_ROOTS)
            .map(|i| PathBuf::from(format!("/sub{i}")))
            .collect();

        for root in &roots {
            remember_root(&plugin, root.clone(), reader_for(root));
        }

        assert_eq!(
            plugin_roots(&plugin),
            roots[1..],
            "the root it asked for longest ago is the one let go"
        );
    }

    /// The root a plugin keeps asking for is the root it keeps, or completing
    /// paths in one directory would eventually drop the directory being
    /// completed.
    #[test]
    fn a_root_asked_for_again_is_the_last_one_to_go() {
        let plugin: Arc<str> = Arc::from(TEST_PLUGIN);
        let roots: Vec<PathBuf> = (0..MAX_PLUGIN_ROOTS)
            .map(|i| PathBuf::from(format!("/held{i}")))
            .collect();
        for root in &roots {
            remember_root(&plugin, root.clone(), reader_for(root));
        }

        remember_root(&plugin, roots[0].clone(), reader_for(&roots[0]));
        let one_more = PathBuf::from("/held_last");
        remember_root(&plugin, one_more.clone(), reader_for(&one_more));

        let held = plugin_roots(&plugin);
        assert!(held.contains(&roots[0]), "asking again kept it");
        assert!(!held.contains(&roots[1]), "and the next oldest went");
    }

    /// A plugin over its root budget is not cut off: it loses the walk it
    /// asked for longest ago, not the answer it is asking for now.
    #[test]
    fn a_plugin_over_its_root_budget_still_gets_an_answer() {
        let base = cwd_root().unwrap();
        let plugin: Arc<str> = Arc::from(TEST_PLUGIN);
        for i in 0..MAX_PLUGIN_ROOTS {
            let spent = base.join(format!("spent{i}"));
            remember_root(&plugin, spent.clone(), reader_for(&spent));
        }

        let tmp = small_tree(&base);
        let lua = Lua::new();
        let tbl = fs_table(&lua);
        let found = files_call(&lua, &tbl, tmp.path(), HIGHLIGHT_QUERY, SMALL_LIMIT);
        assert_eq!(
            found_paths(&found).first().map(String::as_str),
            Some(PICKER_MAIN)
        );
    }

    /// A plugin polling as the user types has to tell an empty answer from an
    /// answer that is not there yet, or it stops asking on a list that was
    /// about to fill, and it has to tell a short list from a small tree. The
    /// still-filling case needs a walk held at an exact point, so it lives
    /// with the `FileIndex::detached` tests.
    #[test]
    fn files_reports_how_much_of_the_tree_it_walked() {
        let base = cwd_root().unwrap();
        let tmp = small_tree(&base);
        let lua = Lua::new();
        let found = files_call(
            &lua,
            &fs_table(&lua),
            tmp.path(),
            HIGHLIGHT_QUERY,
            SMALL_LIMIT,
        );
        assert!(found.get::<bool>("complete").unwrap(), "{SETTLED_WALK}");
        assert!(!found.get::<bool>("crashed").unwrap());
        assert!(!found.get::<bool>("truncated").unwrap());
    }

    /// `FileIndexReady` names the canonical root and Lua cannot canonicalise,
    /// so a plugin that passed a symlinked or relative spelling has no way to
    /// tell whether an event is about its own query. Saying which root the
    /// call settled on is what makes waiting for the event possible at all.
    #[test]
    fn files_answers_with_the_root_it_resolved() {
        let base = cwd_root().unwrap();
        let tmp = small_tree(&base);
        let lua = Lua::new();
        let found = files_call(
            &lua,
            &fs_table(&lua),
            tmp.path(),
            HIGHLIGHT_QUERY,
            SMALL_LIMIT,
        );
        assert_eq!(
            found.get::<String>("root").unwrap(),
            tmp.path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "the same spelling the walk event uses"
        );
    }

    /// Both maps are keyed by plugin name, and a reader left in one of them
    /// keeps its root alive in the host's index forever: the host cannot
    /// evict a root somebody is reading, so an unloaded plugin would go on
    /// pinning a path list the size of its tree and pushing the roots a live
    /// plugin needs out of the registry.
    #[test]
    fn unloading_a_plugin_lets_go_of_the_walks_it_was_reading() {
        let plugin: Arc<str> = Arc::from(UNLOADED_PLUGIN);
        let root = PathBuf::from(FS_ROOT_PATH).join(UNLOADED_PLUGIN);
        remember_root(&plugin, root.clone(), reader_for(&root));
        let token = supersede(&plugin);
        assert_eq!(plugin_roots(&plugin), [root]);

        clear_plugin_files(&plugin);

        assert!(plugin_roots(&plugin).is_empty(), "the reader was let go");
        assert!(
            token.load(Ordering::Relaxed),
            "and the answer nobody is left to publish was given up on"
        );
        assert!(FILE_QUERIES.with_borrow(|live| !live.contains_key(&plugin)));
    }

    /// The index matches its roots against a write by prefix, and its roots
    /// are canonical. A write through a symlinked spelling of one used to
    /// miss it entirely, leaving the picker offering a tree it knew had
    /// moved, and `../sibling/f` kept the `..` that made it look like a write
    /// inside the root it had just climbed out of.
    #[cfg(unix)]
    #[test]
    fn a_touched_path_names_the_tree_it_really_lies_in() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join(REAL_DIR);
        std::fs::create_dir_all(real.join(NESTED_DIR)).unwrap();
        let link = tmp.path().join(LINK_DIR);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let want = real.canonicalize().unwrap().join(TOUCHED_FILE);

        assert_eq!(
            resolved(&link.join(TOUCHED_FILE)),
            want,
            "a write through the symlink is a write to the tree behind it"
        );
        assert_eq!(
            resolved(&real.join(NESTED_DIR).join(ESCAPE_PATH).join(TOUCHED_FILE)),
            want,
            "and a path that climbed back out names where it landed"
        );
        assert_eq!(
            resolved(&link.join(NESTED_DIR).join(ESCAPE_PATH)),
            real.canonicalize().unwrap(),
            "a tail of `..` is a directory like any other, not a path to give up on"
        );
    }

    /// A tail put back by hand is a spelling, and `..` in a spelling is the
    /// directory above the one the link sits in rather than the one it points
    /// into. The tree that moved is the one the kernel would have reached, so
    /// the component is resolved before it is climbed out of. It takes a
    /// vanished ancestor to leave the tail unresolved and a live symlink
    /// inside that tail to make the two answers differ.
    #[cfg(unix)]
    #[test]
    fn a_tail_that_climbs_out_of_a_symlink_names_the_tree_it_points_into() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join(REAL_DIR);
        std::fs::create_dir_all(real.join(NESTED_DIR)).unwrap();
        let link = tmp.path().join(LINK_DIR);
        std::os::unix::fs::symlink(real.join(NESTED_DIR), &link).unwrap();

        assert_eq!(
            resolved(
                &tmp.path()
                    .join(GONE_DIR)
                    .join(ESCAPE_PATH)
                    .join(LINK_DIR)
                    .join(ESCAPE_PATH)
            ),
            real.canonicalize().unwrap(),
            "the `..` climbed out of what the link points into"
        );
    }

    /// The parent is not always there to resolve: `rm` takes a whole tree, and
    /// a write can name a directory that has just gone. Giving up left the raw
    /// spelling, symlinked components and all, and the index matches its roots
    /// against canonical paths, so the tree that moved was told nothing.
    #[cfg(unix)]
    #[test]
    fn a_touched_path_below_a_directory_that_is_gone_still_names_its_tree() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join(REAL_DIR);
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join(LINK_DIR);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(
            resolved(&link.join(NESTED_DIR).join(TOUCHED_FILE)),
            real.canonicalize()
                .unwrap()
                .join(NESTED_DIR)
                .join(TOUCHED_FILE),
            "the nearest ancestor that is there is the one resolved"
        );
    }

    /// A file the agent has just written is a file the user is about to ask
    /// for, and the walk behind the answer can be twenty seconds old.
    #[test]
    fn a_written_file_is_findable_before_the_index_goes_stale() {
        let base = cwd_root().unwrap();
        let tmp = small_tree(&base);
        let lua = Lua::new();
        let tbl = fs_table(&lua);
        // The write has to land on a walk that is over: a walk still running
        // has left no ending for the mark to bring forward, and the loop below
        // would then spin out the whole debounce waiting for one.
        walked(tmp.path());

        let written: mlua::Function = tbl.get("write").unwrap();
        let made = tmp.path().join(NEW_FILE);
        let (ok, err): (Option<bool>, Option<String>) =
            smol::block_on(written.call_async((made.to_str().unwrap(), ""))).unwrap();
        assert_eq!((ok, err), (Some(true), None));

        // The write left a mark on the index rather than a walk, and the
        // mark is the whole of what makes the file findable: `expire` brings
        // the debounce it asked for forward and does nothing at all to a tree
        // nothing marked, so a write that said nothing fails here.
        let index = maki_agent::file_index(tmp.path());
        index.expire();
        let deadline = Instant::now() + WALK_TIMEOUT;
        while !index.corpus().iter().any(|p| p == NEW_FILE) {
            assert!(Instant::now() < deadline, "{NEVER_WALKED}");
            index.refresh();
            std::thread::yield_now();
        }

        assert!(
            found_paths(&files_call(&lua, &tbl, tmp.path(), NEW_FILE, SMALL_LIMIT))
                .iter()
                .any(|p| p == NEW_FILE),
            "the plugin's own call finds it too"
        );
    }
}
