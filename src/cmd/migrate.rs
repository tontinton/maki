use std::collections::HashSet;
use std::env;
use std::fmt::Write;
use std::fs::{self, File};
use std::io::{self, ErrorKind, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};
use maki_storage::paths::{self, tilde};

use crate::provider_scripts::{self, MIGRATE_COMMAND, Reference, Script};

#[cfg(unix)]
const AUTH_FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const AUTH_DIR: &str = "auth";
const WAYLAND_DISPLAY_ENV: &str = "WAYLAND_DISPLAY";

const BACKUP_DIR: &str = ".maki.bak";
const MIGRATION_LOCK_FILE: &str = "legacy-migration.lock";
const TRUST_FILE: &str = "trusted-folders.json";
const TRUST_FOLDERS_KEY: &str = "folders";
const TRUST_REJECTED_KEY: &str = "rejected";
const RECENT_MODELS_FILE: &str = "recent-models";
const SITE_DIR: &str = "site";
const PERMISSIONS_FILE: &str = "permissions.toml";
const STAGED_SUFFIX: &str = ".migrating";
const LOG_PREFIX: &str = "maki.";
const LOG_SUFFIX: &str = ".log";
/// What the old layout shared with config. Everything else in it was written
/// by maki or a plugin through the state dir, so that is where it goes.
const CONFIG_ENTRIES: [&str; 12] = [
    "init.lua",
    "lua",
    PERMISSIONS_FILE,
    "mcp.toml",
    "providers.toml",
    ".env",
    "AGENTS.md",
    "pack-lock.json",
    "commands",
    "skills",
    "themes",
    "providers",
];
/// Locks and caches the new layout rebuilds on its own.
const REBUILT_ENTRIES: [&str; 5] = [
    "maki.log.lock",
    "trusted-folders.lock",
    "models-dev-catalog.json",
    "xai",
    "docs",
];

/// A name the backup can actually take. Storing the file under `x.1` beats
/// overwriting an older backup of the same name.
fn free_path(wanted: &Path) -> PathBuf {
    if !present(wanted) {
        return wanted.to_path_buf();
    }
    let stem = wanted
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    (1..)
        .map(|n| wanted.with_file_name(format!("{stem}.{n}")))
        .find(|candidate| !present(candidate))
        .unwrap_or_else(|| wanted.to_path_buf())
}

fn backup_path(legacy: &Path) -> PathBuf {
    legacy.with_file_name(BACKUP_DIR)
}

fn log_move(name: &str, dst: &Path, note: Option<&str>) {
    match note {
        Some(n) => eprintln!("  {name:<22}-> {} ({n})", tilde(dst)),
        None => eprintln!("  {name:<22}-> {}", tilde(dst)),
    }
}

/// Unlike `exists`, true for a dangling symlink, which still has to move.
fn present(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

fn has_entries(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg_attr(not(unix), expect(unused_variables))]
fn move_file(src: &Path, dst: &Path, legacy: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", tilde(parent)))?;
    }
    // A link to a file in `~/.maki` stays as it is, since that file moves out
    // too and usually lands right next to it.
    #[cfg(unix)]
    if is_symlink(src) && !resolves_into(src, legacy) {
        return relink(src, dst);
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        // Staged so a copy cut short never sits at `dst`, where the next run
        // would take it for the newer file.
        Err(e) if e.kind() == ErrorKind::CrossesDevices => {
            let mut staged = dst.as_os_str().to_owned();
            staged.push(STAGED_SUFFIX);
            fs::copy(src, &staged)
                .with_context(|| format!("copy {} -> {}", tilde(src), tilde(dst)))?;
            fs::rename(&staged, dst).with_context(|| format!("move into {}", tilde(dst)))?;
            fs::remove_file(src).with_context(|| format!("remove source {}", tilde(src)))?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("move {} -> {}", tilde(src), tilde(dst))),
    }
}

/// Both sides are resolved, since `~/.maki` itself can sit behind a symlink.
fn resolves_into(link: &Path, dir: &Path) -> bool {
    let (Ok(target), Ok(dir)) = (fs::canonicalize(link), fs::canonicalize(dir)) else {
        return false;
    };
    target.starts_with(dir)
}

/// `rename` keeps a relative link as it is, so a link out of `~/.maki`, like
/// the ones GNU stow makes, points at nothing from its new home. The new link
/// points at the fully resolved file instead, because even
/// `~/.maki/../dotfiles` breaks once `~/.maki` is gone. A link that already
/// pointed at nothing is copied as is.
#[cfg(unix)]
fn relink(src: &Path, dst: &Path) -> Result<()> {
    let target = fs::canonicalize(src)
        .or_else(|_| fs::read_link(src))
        .with_context(|| format!("read link {}", tilde(src)))?;
    symlink(&target, dst).with_context(|| format!("link {} -> {}", tilde(dst), tilde(&target)))?;
    fs::remove_file(src).with_context(|| format!("remove source {}", tilde(src)))
}

/// Moves `path` into the backup, under its place in the old layout.
fn stash(path: &Path, relative: &Path, legacy: &Path) -> Result<PathBuf> {
    let dst = free_path(&backup_path(legacy).join(relative));
    move_file(path, &dst, legacy)?;
    Ok(dst)
}

/// A link from the new layout into `~/.maki` reaches the very file about to
/// move there, so it looks like a clash and the only copy would go to the
/// backup. It points at nothing once `~/.maki` is gone anyway.
fn unlink_into_legacy(path: &Path, legacy: &Path) -> Result<()> {
    if is_symlink(path) && resolves_into(path, legacy) {
        fs::remove_file(path).with_context(|| format!("remove link {}", tilde(path)))?;
    }
    Ok(())
}

/// One rule for every file both layouts hold and nothing can merge: the copy
/// written last is the user's latest choice and ends up at `new`, the other
/// goes to the backup. Either side can be the newer one, since an old build
/// keeps writing `~/.maki` for as long as that directory exists.
fn keep_newest(old: &Path, new: &Path, legacy: &Path) -> Result<()> {
    if !present(new) {
        return move_file(old, new, legacy);
    }
    let relative = old.strip_prefix(legacy).unwrap_or(old);
    let old_wins = modified(old) > modified(new);
    let stashed = stash(if old_wins { new } else { old }, relative, legacy)?;
    if old_wins {
        move_file(old, new, legacy)?;
    }
    eprintln!(
        "  kept the newer {}, older copy moved to {}",
        tilde(new),
        tilde(&stashed)
    );
    Ok(())
}

/// Recurses rather than renaming directories whole, so a tree on another
/// filesystem still moves and every file on both sides meets `keep_newest`.
fn merge_tree(src: &Path, dst: &Path, legacy: &Path) -> Result<usize> {
    let mut files = 0;
    for entry in fs::read_dir(src).with_context(|| format!("read {}", tilde(src)))? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        unlink_into_legacy(&to, legacy)?;
        if entry.file_type()?.is_dir() {
            files += merge_tree(&from, &to, legacy)?;
        } else {
            keep_newest(&from, &to, legacy)?;
            files += 1;
        }
    }
    fs::remove_dir(src).with_context(|| format!("remove {}", tilde(src)))?;
    Ok(files)
}

fn move_entry(legacy: &Path, target: &Path, name: &str) -> Result<()> {
    let (old, new) = (legacy.join(name), target.join(name));
    let Ok(meta) = fs::symlink_metadata(&old) else {
        return Ok(());
    };
    if meta.is_dir() {
        let files = merge_tree(&old, &new, legacy)?;
        let plural = if files == 1 { "" } else { "s" };
        log_move(
            &format!("{name}/"),
            target,
            Some(&format!("{files} file{plural}")),
        );
        return Ok(());
    }
    let clash = present(&new);
    keep_newest(&old, &new, legacy)?;
    if !clash {
        log_move(name, target, None);
    }
    Ok(())
}

fn remove_entry(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path).ok(),
        Ok(_) => fs::remove_file(path).ok(),
        Err(_) => None,
    };
}

/// Files only: plugin tokens live in a subdirectory, and a directory without
/// its x bit cannot be entered even by its owner.
#[cfg(unix)]
fn restrict_auth(dir: &Path) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_type().is_ok_and(|t| t.is_file()) {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(AUTH_FILE_MODE)).ok();
        }
    }
}

fn move_logs(legacy: &Path, logs_dir: &Path) -> Result<()> {
    let logs: Vec<String> = entry_names(legacy)
        .into_iter()
        .filter(|name| name.starts_with(LOG_PREFIX) && name.ends_with(LOG_SUFFIX))
        .collect();
    for name in &logs {
        keep_newest(&legacy.join(name), &logs_dir.join(name), legacy)?;
    }
    if !logs.is_empty() {
        log_move(
            "maki.*.log",
            logs_dir,
            Some(&format!("{} files", logs.len())),
        );
    }
    Ok(())
}

fn entry_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// The prompt goes to stdout and everything addressed to the user to stderr,
/// so `maki "$(maki migrate providers)"` and a pipe into a clipboard tool
/// carry the prompt and nothing else.
pub fn providers() -> Result<()> {
    let Some(dir) = provider_scripts::providers_dir() else {
        eprintln!("No provider scripts found, so there is nothing to migrate.");
        return Ok(());
    };
    let unported = provider_scripts::unported(&dir);
    if unported.is_empty() {
        eprintln!("{}", nothing_to_port(&dir));
        return Ok(());
    }
    let reference = Reference::find();
    if io::stdout().is_terminal() {
        eprint!(
            "{}",
            providers_banner(&unported, reference.old_maki.as_deref())
        );
    }
    print!("{}", provider_scripts::prompt(&dir, &unported, &reference));
    Ok(())
}

fn nothing_to_port(dir: &Path) -> String {
    if provider_scripts::scripts_in(dir).is_empty() {
        return format!(
            "No provider scripts in {}, so there is nothing to migrate.",
            tilde(dir)
        );
    }
    format!(
        "Every provider script in {0} has a Lua plugin now. Maki ignores the scripts, so you can delete {0} once your plugins no longer read credentials from there.",
        tilde(dir)
    )
}

fn providers_banner(scripts: &[Script], old_maki: Option<&Path>) -> String {
    let width = scripts.iter().map(|s| s.slug.len()).max().unwrap_or(0);
    let mut out =
        String::from("Maki no longer runs provider scripts. These need a Lua plugin:\n\n");
    for script in scripts {
        let _ = writeln!(out, "  {:<width$}  {}", script.slug, tilde(&script.path));
    }
    out.push_str("\nThe prompt below asks a coding agent to port them for you.\n");
    if !cfg!(windows) {
        let run = match old_maki {
            Some(path) => {
                let _ = write!(
                    out,
                    "\nRun it with your previous maki, which `maki update` kept and which still runs the scripts:\n\n  {} \"$({MIGRATE_COMMAND})\"\n",
                    tilde(path)
                );
                "Or run"
            }
            None => "Run",
        };
        let _ = write!(
            out,
            "\n{run} it in maki, with any model you have set up:\n\n  maki \"$({MIGRATE_COMMAND})\"\n"
        );
    }
    let copy = if cfg!(windows) { "Copy" } else { "Or copy" };
    let _ = write!(
        out,
        "\n{copy} it into another agent:\n\n  {MIGRATE_COMMAND} | {}\n\n",
        clipboard_command()
    );
    out
}

fn clipboard_command() -> &'static str {
    if cfg!(target_os = "macos") {
        "pbcopy"
    } else if cfg!(windows) {
        "clip"
    } else if env::var_os(WAYLAND_DISPLAY_ENV).is_some() {
        "wl-copy"
    } else {
        "xclip -selection clipboard"
    }
}

/// Missing new copy: the old one moves in whole. Both present: `merge`
/// combines them into the new one, and an old copy it cannot read goes to the
/// backup rather than over a good new one.
fn reconcile(
    legacy: &Path,
    target: &Path,
    name: &str,
    merge: impl FnOnce(&Path, &Path) -> Result<bool>,
) -> Result<()> {
    let (old, new) = (legacy.join(name), target.join(name));
    if !present(&old) || !present(&new) {
        return move_entry(legacy, target, name);
    }
    if merge(&old, &new)? {
        fs::remove_file(&old).with_context(|| format!("remove {}", tilde(&old)))?;
        log_move(name, target, Some("merged"));
    } else {
        let stashed = stash(&old, Path::new(name), legacy)?;
        eprintln!(
            "  {} could not be merged, moved to {}",
            tilde(&old),
            tilde(&stashed)
        );
    }
    Ok(())
}

fn merge_recent_models(old: &Path, new: &Path) -> Result<bool> {
    let parse =
        |path: &Path| -> Option<Vec<String>> { serde_json::from_slice(&fs::read(path).ok()?).ok() };
    let (Some(current), Some(earlier)) = (parse(new), parse(old)) else {
        return Ok(false);
    };
    let merged = maki_storage::model::merge_recents(&current, &earlier);
    fs::write(new, serde_json::to_vec_pretty(&merged)?)
        .with_context(|| format!("write {}", tilde(new)))?;
    Ok(true)
}

fn merge_trust_files(old: &Path, new: &Path) -> Result<bool> {
    let parse = |path: &Path| -> Option<serde_json::Value> {
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    };
    let (Some(mut merged), Some(old_doc)) = (parse(new), parse(old)) else {
        return Ok(false);
    };

    // A folder and a refusal are two answers to one question, so the lists
    // cannot be merged apart: the newest answer has to win across both.
    let folders = merge_trust_entries(
        &merged,
        &old_doc,
        TRUST_FOLDERS_KEY,
        folder_key,
        &keys_of(&merged, TRUST_REJECTED_KEY, plain_key),
    );
    let rejected = merge_trust_entries(
        &merged,
        &old_doc,
        TRUST_REJECTED_KEY,
        plain_key,
        &keys_of(&merged, TRUST_FOLDERS_KEY, folder_key),
    );
    let Some(object) = merged.as_object_mut() else {
        return Ok(false);
    };
    object.insert(TRUST_FOLDERS_KEY.to_owned(), folders);
    object.insert(TRUST_REJECTED_KEY.to_owned(), rejected);

    fs::write(new, serde_json::to_vec_pretty(&merged)?)
        .with_context(|| format!("write {}", tilde(new)))?;
    Ok(true)
}

fn folder_key(entry: &serde_json::Value) -> Option<String> {
    entry.get("path").map(|path| path.to_string())
}

fn plain_key(entry: &serde_json::Value) -> Option<String> {
    Some(entry.to_string())
}

fn keys_of(
    doc: &serde_json::Value,
    key: &str,
    key_of: impl Fn(&serde_json::Value) -> Option<String>,
) -> HashSet<String> {
    doc.get(key)
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(&key_of)
        .collect()
}

/// New entries come first so a path present in both keeps the newer answer,
/// matching how the document is read. An old entry is dropped when the new
/// file answers the same folder the other way, which `overruled` holds.
fn merge_trust_entries(
    merged: &serde_json::Value,
    old_doc: &serde_json::Value,
    key: &str,
    key_of: impl Fn(&serde_json::Value) -> Option<String>,
    overruled: &HashSet<String>,
) -> serde_json::Value {
    let mut items = merged
        .get(key)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let seen = keys_of(merged, key, &key_of);
    items.extend(
        old_doc
            .get(key)
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter(|entry| {
                key_of(entry).is_none_or(|id| !seen.contains(&id) && !overruled.contains(&id))
            })
            .cloned(),
    );
    serde_json::Value::Array(items)
}

pub fn xdg() -> Result<()> {
    let Some(legacy) = paths::legacy_home_dir() else {
        eprintln!("Nothing to migrate. You are already using XDG directories.");
        return Ok(());
    };
    let _lock = lock_migration()?;
    migrate_from(&legacy)
}

/// Runs before anything else in a start, so the XDG dirs hold nothing this
/// start wrote when `~/.maki` moves into them. Every start does it rather than
/// asking, so no upgrader is left looking at an empty install and this code
/// can go once the old layout is gone. A symlinked `~/.maki` is a dotfiles
/// checkout, which is not maki's to empty.
pub fn startup_notice() -> Option<String> {
    let legacy = paths::legacy_home_dir().filter(|dir| has_entries(dir))?;
    let shown = tilde(&legacy);
    if is_symlink(&legacy) {
        return Some(format!(
            "maki no longer reads {shown}, and it is a symlink, so maki left it alone. \
             Run `maki migrate xdg` to move its files to the XDG directories."
        ));
    }
    let moved = lock_migration().and_then(|_lock| {
        // Another start may have moved it while this one waited.
        if !has_entries(&legacy) {
            return Ok(false);
        }
        migrate_from(&legacy).map(|()| true)
    });
    match moved {
        Ok(false) => None,
        Ok(true) => Some(moved_notice(&legacy)),
        Err(e) => Some(format!(
            "maki no longer reads {shown} and could not move it: {e:#}. \
             Run `maki migrate xdg` to retry."
        )),
    }
}

fn moved_notice(legacy: &Path) -> String {
    let shown = |dir: io::Result<PathBuf>| dir.map(|d| tilde(&d)).unwrap_or_default();
    let mut notice = format!(
        "Moved your maki files from {} to {} and {}, where maki keeps them now.",
        tilde(legacy),
        shown(paths::config_dir()),
        shown(paths::state_dir()),
    );
    let backup = backup_path(legacy);
    if has_entries(&backup) {
        let _ = write!(
            notice,
            " Older copies of files found in both places are in {}.",
            tilde(&backup)
        );
    }
    notice
}

/// Two starts right after an upgrade would otherwise move the same files at
/// once, and the slower one would fail on every file the other already took.
fn lock_migration() -> Result<File> {
    let path = paths::cache_dir()
        .context("determine cache directory")?
        .join(MIGRATION_LOCK_FILE);
    let file = File::create(&path).with_context(|| format!("create {}", tilde(&path)))?;
    file.lock()
        .with_context(|| format!("lock {}", tilde(&path)))?;
    Ok(file)
}

/// The old build read the XDG `permissions.toml` over this one, so the XDG
/// rules are the ones in force. Picking by age instead could swap in a looser
/// file the user never saw take effect.
fn keep_rules_in_force(legacy: &Path, config: &Path) -> Result<()> {
    let shadowed = legacy.join(PERMISSIONS_FILE);
    let in_force = config.join(PERMISSIONS_FILE);
    if present(&shadowed) && present(&in_force) {
        let stashed = stash(&shadowed, Path::new(PERMISSIONS_FILE), legacy)?;
        eprintln!(
            "  kept {}, the rules in force, unused copy moved to {}",
            tilde(&in_force),
            tilde(&stashed)
        );
    }
    Ok(())
}

/// A target inside `~/.maki`, say `~/.config/maki` symlinked to it, holds the
/// very same files, so each one would look like a clash and its only copy
/// would go to the backup before `~/.maki` is removed.
fn ensure_outside(legacy: &Path, targets: &[&Path]) -> Result<()> {
    let legacy_real =
        fs::canonicalize(legacy).with_context(|| format!("resolve {}", tilde(legacy)))?;
    for target in targets {
        let real =
            fs::canonicalize(target).with_context(|| format!("resolve {}", tilde(target)))?;
        if real.starts_with(&legacy_real) {
            let (target, legacy) = (tilde(target), tilde(legacy));
            bail!(
                "{target} points into {legacy}, so maki cannot move {legacy} into it. Move {target} out of {legacy} first"
            );
        }
    }
    Ok(())
}

fn migrate_from(legacy: &Path) -> Result<()> {
    let config = paths::config_dir().context("determine config directory")?;
    let data = paths::data_dir().context("determine data directory")?;
    let state = paths::state_dir().context("determine state directory")?;
    let logs = paths::logs_dir().context("determine log directory")?;
    ensure_outside(legacy, &[&config, &data, &state, &logs])?;
    move_legacy_layout(legacy, &config, &data, &state, &logs)
}

fn move_legacy_layout(
    legacy: &Path,
    config: &Path,
    data: &Path,
    state: &Path,
    logs: &Path,
) -> Result<()> {
    eprintln!("Moving files from {} ...\n", tilde(legacy));

    // `merge_tree` clears the levels below. The top level goes first, since
    // `reconcile` and `keep_rules_in_force` look at both sides too.
    for target in [config, data, state, logs] {
        for name in entry_names(target) {
            unlink_into_legacy(&target.join(name), legacy)?;
        }
    }
    for name in REBUILT_ENTRIES {
        remove_entry(&legacy.join(name));
    }
    reconcile(legacy, state, TRUST_FILE, merge_trust_files)?;
    reconcile(legacy, state, RECENT_MODELS_FILE, merge_recent_models)?;
    keep_rules_in_force(legacy, config)?;
    for name in CONFIG_ENTRIES {
        move_entry(legacy, config, name)?;
    }
    move_entry(legacy, data, SITE_DIR)?;
    move_logs(legacy, logs)?;
    for name in entry_names(legacy) {
        move_entry(legacy, state, &name)?;
    }
    #[cfg(unix)]
    restrict_auth(&state.join(AUTH_DIR));
    let emptied = if is_symlink(legacy) {
        fs::remove_file(legacy)
    } else {
        fs::remove_dir(legacy)
    };
    emptied.with_context(|| format!("remove {}", tilde(legacy)))?;

    eprintln!(
        "\nAll done! Your files now live here:\n\n\
         \x20 Config   {}\n\
         \x20 State    {}\n\
         \x20 Data     {}\n\
         \x20 Logs     {}\n\n\
         Per-project settings (.maki/ in your repos) are not affected.\n\n\
         Removed {}.",
        tilde(config),
        tilde(state),
        tilde(data),
        tilde(logs),
        tilde(legacy),
    );
    let backup = backup_path(legacy);
    if has_entries(&backup) {
        eprintln!(
            "\nOlder copies of files found in both places are in {}. \
             Delete it once you are sure you do not need them.",
            tilde(&backup)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    use test_case::test_case;

    use super::*;

    const AGENTS: &str = "AGENTS.md";
    const AUTH: &str = "auth";
    const INIT_LUA: &str = "init.lua";
    const SESSIONS: &str = "sessions";
    const SESSION_ID: &str = "abc.json";
    const RECENT_MODELS: &str = "recent-models";
    const PACK_LOCK: &str = "pack-lock.json";
    const SITE_FILE: &str = "pack.lua";
    const MCP: &str = "mcp.toml";
    const THEME: &str = "theme";
    const UNKNOWN: &str = "unrecognized.txt";
    const EARLIER_BACKUP: &str = "from an earlier run";
    const OLD_CONTENT: &str = "old";
    const NEW_CONTENT: &str = "new";
    const LONG_AGO: Duration = Duration::from_secs(86_400);
    #[cfg(unix)]
    const PLUGIN_AUTH: &str = "plugins";
    #[cfg(unix)]
    const PERMISSION_BITS: u32 = 0o777;
    #[cfg(unix)]
    const OWNER_EXEC: u32 = 0o100;

    fn xdg_layout(root: &Path) -> [PathBuf; 4] {
        [
            root.join("config"),
            root.join("data"),
            root.join("state"),
            root.join("logs"),
        ]
    }

    fn migrate(legacy: &Path, xdg: &[PathBuf; 4]) -> Result<()> {
        move_legacy_layout(legacy, &xdg[0], &xdg[1], &xdg[2], &xdg[3])
    }

    fn write(path: PathBuf, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn age(path: &Path) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + LONG_AGO)
            .unwrap();
    }

    fn full_legacy_tree(legacy: &Path) {
        write(legacy.join(INIT_LUA), "setup()");
        write(legacy.join(AGENTS), "global rules");
        write(legacy.join(MCP), "[server]");
        write(legacy.join(SESSIONS).join(SESSION_ID), "{}");
        write(legacy.join(TRUST_FILE), r#"{"folders":[]}"#);
        write(legacy.join(RECENT_MODELS), r#"["a/b"]"#);
        write(legacy.join(PACK_LOCK), r#"{"packs":{}}"#);
        write(legacy.join(SITE_DIR).join(SITE_FILE), "return {}");
        write(legacy.join("providers").join("openai.sh"), "echo");
        write(legacy.join(AUTH).join("anthropic"), "{}");
        write(legacy.join(THEME), "dark");
    }

    #[test]
    fn migrating_a_full_legacy_tree_leaves_no_backup() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        full_legacy_tree(&legacy);

        migrate(&legacy, &xdg).unwrap();

        assert!(!legacy.exists(), "the legacy tree is removed");
        assert!(
            !root.path().join(BACKUP_DIR).exists(),
            "every artifact has a home, so nothing is backed up"
        );
        for moved in [
            xdg[0].join(INIT_LUA),
            xdg[0].join(AGENTS),
            xdg[0].join(MCP),
            xdg[0].join(PACK_LOCK),
            xdg[0].join("providers").join("openai.sh"),
            xdg[2].join(SESSIONS).join(SESSION_ID),
            xdg[2].join(TRUST_FILE),
            xdg[2].join(RECENT_MODELS),
            xdg[2].join(AUTH).join("anthropic"),
            xdg[2].join(THEME),
            xdg[1].join(SITE_DIR).join(SITE_FILE),
        ] {
            assert!(moved.exists(), "{} should have moved", tilde(&moved));
        }
    }

    #[test_case(true, OLD_CONTENT, NEW_CONTENT ; "old_copy_written_last_wins")]
    #[test_case(false, NEW_CONTENT, OLD_CONTENT ; "new_copy_written_last_wins")]
    fn a_file_in_both_layouts_keeps_the_newest_and_backs_up_the_other(
        old_is_newer: bool,
        kept: &str,
        backed_up: &str,
    ) {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        for (dir, name) in [(&xdg[0], AGENTS), (&xdg[2], THEME)] {
            write(legacy.join(name), OLD_CONTENT);
            write(dir.join(name), NEW_CONTENT);
            age(&if old_is_newer {
                dir.join(name)
            } else {
                legacy.join(name)
            });
        }

        migrate(&legacy, &xdg).unwrap();

        let backup = root.path().join(BACKUP_DIR);
        for (dir, name) in [(&xdg[0], AGENTS), (&xdg[2], THEME)] {
            assert_eq!(fs::read_to_string(dir.join(name)).unwrap(), kept);
            assert_eq!(fs::read_to_string(backup.join(name)).unwrap(), backed_up);
        }
        assert!(!legacy.exists());
    }

    #[test]
    fn the_permissions_in_force_win_over_a_newer_shadowed_copy() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(legacy.join(PERMISSIONS_FILE), OLD_CONTENT);
        write(xdg[0].join(PERMISSIONS_FILE), NEW_CONTENT);
        age(&xdg[0].join(PERMISSIONS_FILE));

        migrate(&legacy, &xdg).unwrap();

        assert_eq!(
            fs::read_to_string(xdg[0].join(PERMISSIONS_FILE)).unwrap(),
            NEW_CONTENT
        );
        assert_eq!(
            fs::read_to_string(root.path().join(BACKUP_DIR).join(PERMISSIONS_FILE)).unwrap(),
            OLD_CONTENT
        );
    }

    #[test]
    fn a_directory_in_both_layouts_merges_file_by_file() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        let memories = Path::new("projects").join("p1").join("memories");
        write(legacy.join(&memories).join("old.md"), OLD_CONTENT);
        write(xdg[2].join(&memories).join("new.md"), NEW_CONTENT);

        migrate(&legacy, &xdg).unwrap();

        assert!(xdg[2].join(&memories).join("old.md").exists());
        assert!(xdg[2].join(&memories).join("new.md").exists());
        assert!(!root.path().join(BACKUP_DIR).exists());
    }

    #[test]
    fn trust_in_both_layouts_keeps_the_old_folders() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(
            legacy.join(TRUST_FILE),
            r#"{"version":1,"folders":[{"path":"/old","files":["a"]}],"rejected":["/no"]}"#,
        );
        write(
            xdg[2].join(TRUST_FILE),
            r#"{"version":1,"folders":[{"path":"/new","files":["b"]}],"rejected":[]}"#,
        );

        migrate(&legacy, &xdg).unwrap();

        let merged: serde_json::Value =
            serde_json::from_slice(&fs::read(xdg[2].join(TRUST_FILE)).unwrap()).unwrap();
        let folders: Vec<&str> = merged[TRUST_FOLDERS_KEY]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert_eq!(
            folders,
            ["/new", "/old"],
            "an old project still asks nothing"
        );
        assert_eq!(merged[TRUST_REJECTED_KEY].as_array().unwrap().len(), 1);
        assert!(!root.path().join(BACKUP_DIR).join(TRUST_FILE).exists());
    }

    #[test]
    fn a_folder_in_both_trust_files_is_listed_once() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(
            legacy.join(TRUST_FILE),
            r#"{"version":1,"folders":[{"path":"/same","files":["old"]}],"rejected":[]}"#,
        );
        write(
            xdg[2].join(TRUST_FILE),
            r#"{"version":1,"folders":[{"path":"/same","files":["new"]}],"rejected":[]}"#,
        );

        migrate(&legacy, &xdg).unwrap();

        let merged: serde_json::Value =
            serde_json::from_slice(&fs::read(xdg[2].join(TRUST_FILE)).unwrap()).unwrap();
        let folders = merged[TRUST_FOLDERS_KEY].as_array().unwrap();
        assert_eq!(folders.len(), 1, "one entry per path, got {folders:?}");
        assert_eq!(
            folders[0]["files"][0], "new",
            "the newer answer wins, as the first match does when reading"
        );
    }

    #[test]
    fn an_older_copy_the_backup_already_holds_is_stored_under_a_free_name() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        let backup = root.path().join(BACKUP_DIR);
        write(legacy.join(AGENTS), OLD_CONTENT);
        age(&legacy.join(AGENTS));
        write(xdg[0].join(AGENTS), NEW_CONTENT);
        write(backup.join(AGENTS), EARLIER_BACKUP);

        migrate(&legacy, &xdg).unwrap();

        assert_eq!(
            fs::read_to_string(backup.join(AGENTS)).unwrap(),
            EARLIER_BACKUP
        );
        assert_eq!(
            fs::read_to_string(backup.join(format!("{AGENTS}.1"))).unwrap(),
            OLD_CONTENT
        );
        assert!(!legacy.exists());
    }

    #[test]
    fn an_unlisted_entry_is_state_and_rebuilt_ones_are_dropped() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(legacy.join(UNKNOWN), NEW_CONTENT);
        for name in REBUILT_ENTRIES {
            write(legacy.join(name), NEW_CONTENT);
        }

        migrate(&legacy, &xdg).unwrap();

        assert!(xdg[2].join(UNKNOWN).exists());
        for name in REBUILT_ENTRIES {
            assert!(!xdg[2].join(name).exists(), "{name} is rebuilt, not moved");
        }
        assert!(!legacy.exists());
    }

    #[test]
    fn a_decision_made_after_upgrading_overrules_the_old_one() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(
            legacy.join(TRUST_FILE),
            r#"{"version":1,"folders":[{"path":"/kept","files":[]},{"path":"/refused-after","files":[]}],"rejected":["/allowed-after"]}"#,
        );
        write(
            xdg[2].join(TRUST_FILE),
            r#"{"version":1,"folders":[{"path":"/allowed-after","files":[]}],"rejected":["/refused-after"]}"#,
        );

        migrate(&legacy, &xdg).unwrap();

        let merged: serde_json::Value =
            serde_json::from_slice(&fs::read(xdg[2].join(TRUST_FILE)).unwrap()).unwrap();
        let folders: Vec<&str> = merged[TRUST_FOLDERS_KEY]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["path"].as_str())
            .collect();
        let rejected: Vec<&str> = merged[TRUST_REJECTED_KEY]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry.as_str())
            .collect();

        assert_eq!(
            folders,
            ["/allowed-after", "/kept"],
            "a folder refused in the new file must not come back as allowed"
        );
        assert_eq!(
            rejected,
            ["/refused-after"],
            "a folder allowed in the new file must not stay refused"
        );
    }

    #[test]
    fn recent_models_in_both_layouts_keep_the_old_picks() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(legacy.join(RECENT_MODELS), r#"["old/a","old/b"]"#);
        write(xdg[2].join(RECENT_MODELS), r#"["new/c"]"#);

        migrate(&legacy, &xdg).unwrap();

        let merged: Vec<String> =
            serde_json::from_slice(&fs::read(xdg[2].join(RECENT_MODELS)).unwrap()).unwrap();
        assert_eq!(merged, ["new/c", "old/a", "old/b"]);
        assert!(!root.path().join(BACKUP_DIR).join(RECENT_MODELS).exists());
    }

    #[test]
    fn a_damaged_old_trust_file_does_not_abort_the_run() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(legacy.join(TRUST_FILE), "not json");
        write(xdg[2].join(TRUST_FILE), r#"{"folders":[]}"#);
        write(legacy.join(INIT_LUA), "setup()");
        migrate(&legacy, &xdg).unwrap();
        assert!(
            xdg[0].join(INIT_LUA).exists(),
            "migration continued past the bad trust file"
        );
        assert!(root.path().join(BACKUP_DIR).join(TRUST_FILE).exists());
    }

    #[test_case("config", true ; "a_target_outside_is_fine")]
    #[test_case(".maki/config", false ; "a_target_inside_is_refused")]
    fn targets_must_be_outside_the_legacy_dir(target: &str, allowed: bool) {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let target = root.path().join(target);
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&target).unwrap();

        assert_eq!(ensure_outside(&legacy, &[&target]).is_ok(), allowed);
    }

    #[cfg(unix)]
    #[test_case("../dotfiles/init.lua" ; "a_link_out_of_the_old_dir")]
    #[test_case("lua/main.lua" ; "a_link_within_the_old_dir")]
    fn a_relative_symlink_still_reaches_its_file_after_the_move(target: &str) {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        fs::create_dir_all(&legacy).unwrap();
        write(legacy.join(target), NEW_CONTENT);
        symlink(target, legacy.join(INIT_LUA)).unwrap();

        migrate(&legacy, &xdg).unwrap();

        assert_eq!(
            fs::read_to_string(xdg[0].join(INIT_LUA)).unwrap(),
            NEW_CONTENT
        );
    }

    #[cfg(unix)]
    #[test_case(0, INIT_LUA, INIT_LUA ; "a_link_to_a_file")]
    #[test_case(2, SESSIONS, "sessions/abc.json" ; "a_link_to_a_directory")]
    #[test_case(2, "sessions/abc.json", "sessions/abc.json" ; "a_link_inside_a_merged_directory")]
    #[test_case(0, PERMISSIONS_FILE, PERMISSIONS_FILE ; "a_link_to_the_permissions")]
    #[test_case(2, TRUST_FILE, TRUST_FILE ; "a_link_to_a_merged_file")]
    fn a_link_into_the_old_dir_is_replaced_by_the_real_file(
        target: usize,
        linked: &str,
        file: &str,
    ) {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(legacy.join(file), NEW_CONTENT);
        let link = xdg[target].join(linked);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(legacy.join(linked), link).unwrap();

        migrate(&legacy, &xdg).unwrap();

        assert_eq!(
            fs::read_to_string(xdg[target].join(file)).unwrap(),
            NEW_CONTENT
        );
        assert!(!root.path().join(BACKUP_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn auth_files_are_restricted_and_the_plugin_dir_stays_enterable() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(legacy.join(AUTH).join("anthropic"), "{}");
        write(legacy.join(AUTH).join(PLUGIN_AUTH).join("acme.json"), "{}");

        migrate(&legacy, &xdg).unwrap();

        let mode =
            |path: PathBuf| fs::metadata(path).unwrap().permissions().mode() & PERMISSION_BITS;
        let auth = xdg[2].join(AUTH);
        assert_eq!(mode(auth.join("anthropic")), AUTH_FILE_MODE);
        assert_ne!(
            mode(auth.join(PLUGIN_AUTH)) & OWNER_EXEC,
            0,
            "plugin logins live in there"
        );
    }
}
