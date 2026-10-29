use std::collections::HashSet;
use std::env;
use std::fmt::Write;
use std::fs;
use std::io::{self, ErrorKind, IsTerminal};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use color_eyre::Result;
use color_eyre::eyre::Context;
use maki_storage::input_history::MAX_ENTRIES;
use maki_storage::paths::{self, tilde};

use crate::provider_scripts::{self, MIGRATE_COMMAND, Reference, Script};

#[cfg(unix)]
const AUTH_FILE_MODE: u32 = 0o600;
const WAYLAND_DISPLAY_ENV: &str = "WAYLAND_DISPLAY";

const BACKUP_DIR: &str = ".maki.bak";
const TRUST_FILE: &str = "trusted-folders.json";
const TRUST_LOCK_FILE: &str = "trusted-folders.lock";
const TRUST_FOLDERS_KEY: &str = "folders";
const TRUST_REJECTED_KEY: &str = "rejected";
const PRE_TRUST_FILE: &str = "pre-trust-roots.json";
const SESSIONS_DIR: &str = "sessions";
const RECENT_MODELS_FILE: &str = "recent-models";
const PACK_LOCK_FILE: &str = "pack-lock.json";
const SITE_DIR: &str = "site";

/// A name the backup can actually take. Storing the file under `x.1` beats
/// leaving it in `~/.maki`, which would keep the tree (and its warning) alive.
fn free_path(wanted: &Path) -> PathBuf {
    if !wanted.exists() {
        return wanted.to_path_buf();
    }
    let stem = wanted
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    (1..)
        .map(|n| wanted.with_file_name(format!("{stem}.{n}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or_else(|| wanted.to_path_buf())
}

/// The copy that stays is the one at `new`; the old one is left in place, and
/// whatever the caller does not move ends up in the backup at the end.
fn kept_line(new: &Path, old: &Path, legacy: &Path) -> String {
    let left = match old.strip_prefix(legacy) {
        Ok(rest) => backup_path(legacy).join(rest),
        Err(_) => old.to_path_buf(),
    };
    format!("kept {}, old copy left at {}", tilde(new), tilde(&left))
}

fn backup_path(legacy: &Path) -> PathBuf {
    legacy.with_file_name(BACKUP_DIR)
}

fn log_kept(new: &Path, old: &Path, legacy: &Path) {
    println!("  {}", kept_line(new, old, legacy));
}

fn log_move(name: &str, dst: &Path, note: Option<&str>) {
    match note {
        Some(n) => println!("  {name:<22}-> {} ({n})", tilde(dst)),
        None => println!("  {name:<22}-> {}", tilde(dst)),
    }
}

fn move_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", tilde(parent)))?;
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::CrossesDevices => {
            fs::copy(src, dst).with_context(|| format!("copy {} -> {}", tilde(src), tilde(dst)))?;
            #[cfg(unix)]
            {
                let mode = fs::metadata(src)
                    .map(|m| m.permissions().mode())
                    .unwrap_or(0o644);
                fs::set_permissions(dst, fs::Permissions::from_mode(mode)).ok();
            }
            fs::remove_file(src).with_context(|| format!("remove source {}", tilde(src)))?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("move {} -> {}", tilde(src), tilde(dst))),
    }
}

fn move_auth(legacy_dir: &Path, target_dir: &Path) -> Result<()> {
    if !legacy_dir.is_dir() {
        return Ok(());
    }

    let entries: Vec<_> = fs::read_dir(legacy_dir)
        .with_context(|| format!("read {}", tilde(legacy_dir)))?
        .filter_map(|e| e.ok())
        .collect();

    if entries.is_empty() {
        return Ok(());
    }

    fs::create_dir_all(target_dir).with_context(|| format!("create {}", tilde(target_dir)))?;

    let count = entries.len();
    let mut replaced = 0;
    for entry in &entries {
        let dst = target_dir.join(entry.file_name());
        if dst.exists() {
            replaced += 1;
            fs::remove_file(&dst).ok();
        }
        move_file(&entry.path(), &dst)?;
        #[cfg(unix)]
        fs::set_permissions(&dst, fs::Permissions::from_mode(AUTH_FILE_MODE)).ok();
    }
    fs::remove_dir(legacy_dir).ok();

    let plural = if count == 1 { "" } else { "s" };
    let mut note = format!("{count} file{plural}");
    if replaced > 0 {
        let _ = write!(note, ", {replaced} replaced the newer copy");
    }
    log_move("auth/", target_dir, Some(&note));
    Ok(())
}

fn merge_json_file(legacy: &Path, target: &Path, name: &str) -> Result<()> {
    if !legacy.exists() {
        return Ok(());
    }

    if !target.exists() {
        move_file(legacy, target)?;
        log_move(name, target.parent().unwrap_or(target), None);
        return Ok(());
    }

    let legacy_bytes = fs::read(legacy)?;
    let target_bytes = fs::read(target)?;

    let mut merged: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&target_bytes).unwrap_or_default();
    let legacy_map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&legacy_bytes).unwrap_or_default();
    merged.extend(legacy_map);

    fs::write(target, serde_json::to_vec_pretty(&merged)?)
        .with_context(|| format!("write {}", tilde(target)))?;
    fs::remove_file(legacy)?;
    log_move(name, target.parent().unwrap_or(target), Some("merged"));
    Ok(())
}

fn merge_input_history(legacy: &Path, target: &Path) -> Result<()> {
    if !legacy.exists() {
        return Ok(());
    }

    let legacy_items: Vec<String> = serde_json::from_slice(&fs::read(legacy)?).unwrap_or_default();

    if !target.exists() {
        move_file(legacy, target)?;
        log_move(
            "input_history.json",
            target.parent().unwrap_or(target),
            Some(&format!("{} entries", legacy_items.len())),
        );
        return Ok(());
    }

    let target_items: Vec<String> = serde_json::from_slice(&fs::read(target)?).unwrap_or_default();

    let mut merged = Vec::with_capacity(target_items.len() + legacy_items.len());
    merged.extend(target_items);
    merged.extend(legacy_items);
    merged.dedup();
    merged.truncate(MAX_ENTRIES);

    fs::write(target, serde_json::to_vec(&merged)?)
        .with_context(|| format!("write {}", tilde(target)))?;
    fs::remove_file(legacy)?;
    log_move(
        "input_history.json",
        target.parent().unwrap_or(target),
        Some(&format!("merged, {} entries", merged.len())),
    );
    Ok(())
}

fn merge_dir(legacy: &Path, target: &Path, subdir: &str, recursive: bool) -> Result<(u32, u32)> {
    let src = legacy.join(subdir);
    let dst = target.join(subdir);
    if !src.is_dir() {
        return Ok((0, 0));
    }
    fs::create_dir_all(&dst).with_context(|| format!("create {}", tilde(&dst)))?;

    let mut moved = 0u32;
    let mut skipped = 0u32;

    for entry in fs::read_dir(&src).with_context(|| format!("read {}", tilde(&src)))? {
        let entry = entry?;
        let entry_dst = dst.join(entry.file_name());

        if entry_dst.exists() {
            if recursive && entry.file_type()?.is_dir() {
                let sub = format!("{subdir}/{}", entry.file_name().to_string_lossy());
                let (m, s) = merge_dir(legacy, target, &sub, true)?;
                moved += m;
                skipped += s;
            } else {
                log_kept(&entry_dst, &entry.path(), legacy);
                skipped += 1;
            }
        } else {
            move_file(&entry.path(), &entry_dst)?;
            moved += 1;
        }
    }

    fs::remove_dir(&src).ok();

    if moved > 0 || skipped > 0 {
        let kind = if recursive { "dirs" } else { "files" };
        let mut note = format!("{moved} {kind}");
        if skipped > 0 {
            note.push_str(&format!(", {skipped} skipped"));
        }
        log_move(&format!("{subdir}/"), &dst, Some(&note));
    }

    Ok((moved, skipped))
}

fn move_logs(legacy: &Path, logs_dir: &Path) -> Result<()> {
    let entries: Vec<_> = fs::read_dir(legacy)
        .with_context(|| format!("read {}", tilde(legacy)))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("maki.") && name.ends_with(".log")
        })
        .collect();

    if entries.is_empty() {
        return Ok(());
    }

    fs::create_dir_all(logs_dir).with_context(|| format!("create {}", tilde(logs_dir)))?;

    for entry in &entries {
        let dst = logs_dir.join(entry.file_name());
        let name = entry.file_name();
        if dst.exists() {
            println!("  {} (skipped, already exists)", name.to_string_lossy());
        } else {
            move_file(&entry.path(), &dst)?;
            log_move(&name.to_string_lossy(), logs_dir, None);
        }
    }
    Ok(())
}

fn list_remaining(dir: &Path) -> Vec<String> {
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

fn moved_name(src: &Path) -> String {
    src.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Missing old copy: nothing to do. Missing new copy: the old one moves in
/// whole. Both present: the caller decides, because only it knows whether
/// "both" means merge or keep.
fn reconcile(
    legacy: &Path,
    old: &Path,
    new: &Path,
    merge: impl FnOnce(&Path, &Path) -> Result<()>,
) -> Result<()> {
    if !old.exists() {
        return Ok(());
    }
    if !new.exists() {
        return move_if_absent(old, new, legacy);
    }
    merge(old, new)
}

/// A first launch before migrating can rewrite these, so the old copy is not
/// simply redundant: merging keeps what only the old layout knew.
fn merge_recent_models(legacy: &Path, state: &Path) -> Result<()> {
    reconcile(
        legacy,
        &legacy.join(RECENT_MODELS_FILE),
        &state.join(RECENT_MODELS_FILE),
        |old, new| merge_recent_files(state, old, new),
    )
}

fn merge_recent_files(state: &Path, old: &Path, new: &Path) -> Result<()> {
    let current: Vec<String> =
        serde_json::from_slice(&fs::read(new)?).with_context(|| format!("parse {}", tilde(new)))?;
    let earlier: Vec<String> = match serde_json::from_slice(&fs::read(old)?) {
        Ok(list) => list,
        Err(_) => {
            println!("  {} (not a model list, left for the backup)", tilde(old));
            return Ok(());
        }
    };

    let merged = maki_storage::model::merge_recents(&current, &earlier);
    fs::write(new, serde_json::to_vec_pretty(&merged)?)
        .with_context(|| format!("write {}", tilde(new)))?;
    fs::remove_file(old).ok();
    log_move(RECENT_MODELS_FILE, state, Some("merged, old picks kept"));
    Ok(())
}

/// Trust is the one artifact where the new file is not authoritative: a first
/// launch before migrating writes a fresh one, so dropping the old entries
/// makes every project ask again.
fn merge_trust(legacy: &Path, state: &Path) -> Result<()> {
    reconcile(
        legacy,
        &legacy.join(TRUST_FILE),
        &state.join(TRUST_FILE),
        |old, new| merge_trust_files(state, old, new),
    )
}

fn merge_trust_files(state: &Path, old: &Path, new: &Path) -> Result<()> {
    // A damaged side must not abort the run: leave the old file where it is,
    // and the backup step at the end names it.
    let parse = |path: &Path| -> Option<serde_json::Value> {
        let bytes = fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    };
    let (Some(mut merged), Some(old_doc)) = (parse(new), parse(old)) else {
        println!("  {} (not valid JSON, left for the backup)", tilde(old));
        return Ok(());
    };
    if !merged.is_object() {
        println!(
            "  {} (not a trust document, left for the backup)",
            tilde(old)
        );
        return Ok(());
    }

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
        return Ok(());
    };
    object.insert(TRUST_FOLDERS_KEY.to_owned(), folders);
    object.insert(TRUST_REJECTED_KEY.to_owned(), rejected);

    fs::write(new, serde_json::to_vec_pretty(&merged)?)
        .with_context(|| format!("write {}", tilde(new)))?;
    fs::remove_file(old).ok();
    log_move(TRUST_FILE, state, Some("merged, old entries kept"));
    Ok(())
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

/// A plain config file has nothing to merge, so the new copy wins and the old
/// one is left alone rather than clobbered.
fn move_if_absent(src: &Path, dst: &Path, legacy: &Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    if dst.exists() {
        log_kept(dst, src, legacy);
        return Ok(());
    }
    move_file(src, dst)?;
    log_move(&moved_name(src), dst.parent().unwrap_or(dst), None);
    Ok(())
}

pub fn xdg() -> Result<()> {
    let Some(legacy) = paths::legacy_home_dir() else {
        println!("Nothing to migrate. You are already using XDG directories.");
        return Ok(());
    };

    let config = paths::config_dir().context("determine config directory")?;
    let data = paths::data_dir().context("determine data directory")?;
    let state = paths::state_dir().context("determine state directory")?;
    let logs = paths::logs_dir().context("determine log directory")?;

    move_legacy_layout(&legacy, &config, &data, &state, &logs)
}

fn move_legacy_layout(
    legacy: &Path,
    config: &Path,
    data: &Path,
    state: &Path,
    logs: &Path,
) -> Result<()> {
    fs::create_dir_all(data).with_context(|| format!("create {}", tilde(data)))?;

    println!("Moving files from ~/.maki/ ...\n");

    move_auth(&legacy.join("auth"), &state.join("auth"))?;

    merge_dir(legacy, state, SESSIONS_DIR, false)?;
    // A first launch before migrating writes this snapshot from the session
    // index it could see, so the pre-XDG one is the complete answer. The merge
    // above kept the newer copy; drop it here so the old one can land.
    let pre_trust = state.join(SESSIONS_DIR).join(PRE_TRUST_FILE);
    if legacy.join(SESSIONS_DIR).join(PRE_TRUST_FILE).exists() {
        fs::remove_file(&pre_trust).ok();
        move_if_absent(
            &legacy.join(SESSIONS_DIR).join(PRE_TRUST_FILE),
            &pre_trust,
            legacy,
        )?;
    }
    // `merge_dir` removed this before the move above emptied it, and a bare
    // empty directory reads as an unrecognized file to the backup step.
    fs::remove_dir(legacy.join(SESSIONS_DIR)).ok();
    merge_dir(legacy, state, "plans", false)?;
    merge_dir(legacy, state, "projects", true)?;
    merge_dir(legacy, config, "providers", false)?;

    for name in [
        "init.lua",
        "permissions.toml",
        "mcp.toml",
        "providers.toml",
        ".env",
        "AGENTS.md",
    ] {
        move_if_absent(&legacy.join(name), &config.join(name), legacy)?;
    }
    merge_dir(legacy, config, "commands", false)?;
    merge_dir(legacy, config, "themes", false)?;
    move_if_absent(
        &legacy.join(PACK_LOCK_FILE),
        &config.join(PACK_LOCK_FILE),
        legacy,
    )?;

    merge_trust(legacy, state)?;
    merge_recent_models(legacy, state)?;
    move_if_absent(
        &legacy.join(TRUST_LOCK_FILE),
        &state.join(TRUST_LOCK_FILE),
        legacy,
    )?;
    merge_dir(legacy, data, SITE_DIR, true)?;

    merge_json_file(
        &legacy.join("cwd_latest.json"),
        &state.join("cwd_latest.json"),
        "cwd_latest.json",
    )?;
    merge_input_history(
        &legacy.join("input_history.json"),
        &state.join("input_history.json"),
    )?;
    merge_json_file(
        &legacy.join("model-tiers"),
        &state.join("model-tiers"),
        "model-tiers",
    )?;

    for name in ["theme", "model"] {
        let src = &legacy.join(name);
        if !src.exists() {
            continue;
        }
        let dst = &state.join(name);
        // The old value is the deliberate pick, so it wins, but replacing a
        // newer one is a decision the user has to be able to see.
        let note = dst.exists().then_some("replaced the newer copy");
        if note.is_some() {
            fs::remove_file(dst).with_context(|| format!("remove existing {}", tilde(dst)))?;
        }
        move_file(src, dst)?;
        log_move(name, dst.parent().unwrap_or(dst), note);
    }

    move_logs(legacy, logs)?;

    let lock_file = &legacy.join("maki.log.lock");
    if lock_file.exists() {
        fs::remove_file(lock_file).ok();
    }

    let remaining = list_remaining(legacy);
    let has_leftovers = !remaining.is_empty();
    let backup = backup_path(legacy);
    let mut left_behind = false;
    if has_leftovers {
        fs::create_dir_all(&backup).context("create ~/.maki.bak/")?;
        for name in &remaining {
            let src = legacy.join(name);
            let dst = free_path(&backup.join(name));
            if let Err(e) = fs::rename(&src, &dst) {
                eprintln!("  warning: could not move {name} to ~/.maki.bak/: {e}");
                left_behind = true;
            }
        }
    }

    // Deleting the tree is what would lose a file the backup step could not
    // take, so it only happens once every leftover is safely out. A taken name
    // is not one of those cases: `free_path` moves it aside.
    if left_behind {
        println!(
            "\nLeft ~/.maki/ in place: some files could not be moved to {}.",
            tilde(&backup)
        );
        return Ok(());
    }

    if legacy.is_dir() {
        fs::remove_dir_all(legacy).context("remove ~/.maki/")?;
    }

    println!(
        "\nAll done! Your files now live here:\n\n\
         \x20 Config   {}\n\
         \x20          init.lua, permissions.toml, mcp.toml, providers.toml,\n\
         \x20          .env, AGENTS.md, commands/, themes/, providers/\n\n\
         \x20 State    {}\n\
         \x20          sessions, auth, plans, memories, input history, preferences\n\n\
         \x20 Logs     {}\n\n\
         Per-project settings (.maki/ in your repos) are not affected.\n\n\
         Removed ~/.maki/.",
        tilde(config),
        tilde(state),
        tilde(logs),
    );

    if has_leftovers {
        let backup_remaining = list_remaining(&backup);
        if backup_remaining.is_empty() {
            fs::remove_dir(&backup).ok();
        } else {
            println!(
                "\nSome unrecognized files were moved to {}:\n",
                tilde(&backup)
            );
            for name in &backup_remaining {
                println!("  {name}");
            }
            println!("\nFeel free to delete them if you do not need them.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const AGENTS: &str = "AGENTS.md";
    const INIT_LUA: &str = "init.lua";
    const SESSIONS: &str = "sessions";
    const SESSION_ID: &str = "abc.json";
    const TRUST: &str = "trusted-folders.json";
    const RECENT_MODELS: &str = "recent-models";
    const PACK_LOCK: &str = "pack-lock.json";
    const SITE: &str = "site";
    const SITE_FILE: &str = "pack.lua";
    const MCP: &str = "mcp.toml";
    const THEME: &str = "theme";
    const UNKNOWN: &str = "unrecognized.txt";

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

    fn write_json(path: PathBuf, content: &str) {
        write(path, content);
    }

    fn full_legacy_tree(legacy: &Path) {
        write(legacy.join(INIT_LUA), "setup()");
        write(legacy.join(AGENTS), "global rules");
        write(legacy.join(MCP), "[server]");
        write(legacy.join(SESSIONS).join(SESSION_ID), "{}");
        write_json(legacy.join(TRUST), r#"{"folders":[]}"#);
        write_json(legacy.join(RECENT_MODELS), r#"["a/b"]"#);
        write_json(legacy.join(PACK_LOCK), r#"{"packs":{}}"#);
        write(legacy.join(SITE).join(SITE_FILE), "return {}");
        write(legacy.join("providers").join("openai.sh"), "echo");
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
            !root.path().join(".maki.bak").exists(),
            "every artifact has a home, so nothing is backed up"
        );
        for moved in [
            xdg[0].join(INIT_LUA),
            xdg[0].join(AGENTS),
            xdg[0].join(MCP),
            xdg[0].join(PACK_LOCK),
            xdg[0].join("providers").join("openai.sh"),
            xdg[2].join(SESSIONS).join(SESSION_ID),
            xdg[2].join(TRUST),
            xdg[2].join(RECENT_MODELS),
            xdg[1].join(SITE).join(SITE_FILE),
        ] {
            assert!(moved.exists(), "{} should have moved", tilde(&moved));
        }
    }

    #[test]
    fn a_file_in_both_places_is_reported_and_the_new_copy_kept() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(legacy.join(AGENTS), "old rules");
        write(xdg[0].join(AGENTS), "new rules");

        migrate(&legacy, &xdg).unwrap();

        let line = kept_line(&xdg[0].join(AGENTS), &legacy.join(AGENTS), &legacy);
        let backup = root.path().join(".maki.bak");
        assert!(
            line.contains(&tilde(&backup.join(AGENTS))),
            "the report names where the old copy really ends up, got {line}"
        );
        assert!(
            !line.contains(&tilde(&legacy.join(AGENTS))),
            "the legacy path is removed at the end, so naming it is misleading, got {line}"
        );
        assert_eq!(
            fs::read_to_string(xdg[0].join(AGENTS)).unwrap(),
            "new rules",
            "the new copy is untouched"
        );
        assert_eq!(
            fs::read_to_string(backup.join(AGENTS)).unwrap(),
            "old rules",
            "the old copy is the one in the backup"
        );
    }

    #[test]
    fn trust_written_before_migrating_keeps_the_old_folders() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write_json(
            legacy.join(TRUST),
            r#"{"version":1,"folders":[{"path":"/old","files":["a"]}],"rejected":["/no"]}"#,
        );
        write_json(
            xdg[2].join(TRUST),
            r#"{"version":1,"folders":[{"path":"/new","files":["b"]}],"rejected":[]}"#,
        );

        migrate(&legacy, &xdg).unwrap();

        let merged: serde_json::Value =
            serde_json::from_slice(&fs::read(xdg[2].join(TRUST)).unwrap()).unwrap();
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
        assert!(!root.path().join(".maki.bak").join(TRUST).exists());
    }

    #[test]
    fn moving_global_config_leaves_no_backup() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(legacy.join(INIT_LUA), "setup()");
        write(legacy.join(AGENTS), "global rules");

        migrate(&legacy, &xdg).unwrap();

        assert!(xdg[0].join(INIT_LUA).exists());
        assert!(xdg[0].join(AGENTS).exists());
        assert!(!root.path().join(".maki.bak").exists());
    }
    #[test]
    fn a_folder_in_both_trust_files_is_listed_once() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write_json(
            legacy.join(TRUST),
            r#"{"version":1,"folders":[{"path":"/same","files":["old"]}],"rejected":[]}"#,
        );
        write_json(
            xdg[2].join(TRUST),
            r#"{"version":1,"folders":[{"path":"/same","files":["new"]}],"rejected":[]}"#,
        );

        migrate(&legacy, &xdg).unwrap();

        let merged: serde_json::Value =
            serde_json::from_slice(&fs::read(xdg[2].join(TRUST)).unwrap()).unwrap();
        let folders = merged[TRUST_FOLDERS_KEY].as_array().unwrap();
        assert_eq!(folders.len(), 1, "one entry per path, got {folders:?}");
        assert_eq!(
            folders[0]["files"][0], "new",
            "the newer answer wins, as the first match does when reading"
        );
    }

    #[test]
    fn a_file_the_backup_cannot_take_is_stored_under_a_free_name() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        let backup = root.path().join(BACKUP_DIR);
        write(legacy.join(UNKNOWN), "newer");
        write(backup.join(UNKNOWN), "older");

        migrate(&legacy, &xdg).unwrap();

        assert_eq!(
            fs::read_to_string(backup.join(UNKNOWN)).unwrap(),
            "older",
            "the existing backup entry is untouched"
        );
        assert_eq!(
            fs::read_to_string(backup.join(format!("{UNKNOWN}.1"))).unwrap(),
            "newer",
            "the newer file is kept, not deleted with the tree"
        );
        assert!(!legacy.exists(), "nothing is left to warn about");
    }

    #[test]
    fn a_decision_made_after_upgrading_overrules_the_old_one() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write_json(
            legacy.join(TRUST),
            r#"{"version":1,"folders":[{"path":"/kept","files":[]},{"path":"/refused-after","files":[]}],"rejected":["/allowed-after"]}"#,
        );
        write_json(
            xdg[2].join(TRUST),
            r#"{"version":1,"folders":[{"path":"/allowed-after","files":[]}],"rejected":["/refused-after"]}"#,
        );

        migrate(&legacy, &xdg).unwrap();

        let merged: serde_json::Value =
            serde_json::from_slice(&fs::read(xdg[2].join(TRUST)).unwrap()).unwrap();
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
    fn an_old_pre_trust_snapshot_replaces_the_new_one_and_leaves_no_backup() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(legacy.join(SESSIONS).join(SESSION_ID), "{}");
        write(
            legacy.join(SESSIONS).join(PRE_TRUST_FILE),
            r#"["/old-root"]"#,
        );
        write(
            xdg[2].join(SESSIONS).join(PRE_TRUST_FILE),
            r#"["/new-root"]"#,
        );

        migrate(&legacy, &xdg).unwrap();

        assert_eq!(
            fs::read_to_string(xdg[2].join(SESSIONS).join(PRE_TRUST_FILE)).unwrap(),
            r#"["/old-root"]"#,
            "the pre-XDG snapshot is the complete answer"
        );
        assert!(xdg[2].join(SESSIONS).join(SESSION_ID).exists());
        assert!(
            !root.path().join(".maki.bak").exists(),
            "the emptied sessions dir must not read as an unrecognized file"
        );
    }

    #[test]
    fn an_older_theme_wins_and_is_not_backed_up() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(legacy.join(THEME), "dark");
        write(xdg[2].join(THEME), "light");

        migrate(&legacy, &xdg).unwrap();

        assert_eq!(
            fs::read_to_string(xdg[2].join(THEME)).unwrap(),
            "dark",
            "the pre-XDG pick is the deliberate one"
        );
        assert!(!root.path().join(".maki.bak").join(THEME).exists());
    }

    #[test]
    fn recent_models_written_before_migrating_keep_the_old_picks() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write_json(legacy.join(RECENT_MODELS), r#"["old/a","old/b"]"#);
        write_json(xdg[2].join(RECENT_MODELS), r#"["new/c"]"#);

        migrate(&legacy, &xdg).unwrap();

        let merged: Vec<String> =
            serde_json::from_slice(&fs::read(xdg[2].join(RECENT_MODELS)).unwrap()).unwrap();
        assert_eq!(merged, ["new/c", "old/a", "old/b"]);
        assert!(!root.path().join(".maki.bak").join(RECENT_MODELS).exists());
    }

    #[test]
    fn a_damaged_old_trust_file_does_not_abort_the_run() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".maki");
        let xdg = xdg_layout(root.path());
        write(legacy.join(TRUST), "not json");
        write(xdg[2].join(TRUST), r#"{"folders":[]}"#);
        write(legacy.join(INIT_LUA), "setup()");
        migrate(&legacy, &xdg).unwrap();
        assert!(
            xdg[0].join(INIT_LUA).exists(),
            "migration continued past the bad trust file"
        );
        assert!(root.path().join(".maki.bak").join(TRUST).exists());
    }
}
