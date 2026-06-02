use std::fs;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use color_eyre::Result;
use color_eyre::eyre::Context;
use maki_storage::input_history::MAX_ENTRIES;
use maki_storage::paths;

#[cfg(unix)]
const AUTH_FILE_MODE: u32 = 0o600;

fn tilde(path: &Path) -> String {
    match paths::home() {
        Some(home) if path.starts_with(&home) => {
            format!("~/{}", path.strip_prefix(&home).unwrap().display())
        }
        _ => path.display().to_string(),
    }
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
        Err(e) if is_cross_device(&e) => {
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

#[cfg(unix)]
fn is_cross_device(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::EXDEV)
}

#[cfg(windows)]
fn is_cross_device(e: &std::io::Error) -> bool {
    // ERROR_NOT_SAME_DEVICE
    e.raw_os_error() == Some(17)
}

#[cfg(not(any(unix, windows)))]
fn is_cross_device(_e: &std::io::Error) -> bool {
    false
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
    for entry in &entries {
        let dst = target_dir.join(entry.file_name());
        if dst.exists() {
            fs::remove_file(&dst).ok();
        }
        move_file(&entry.path(), &dst)?;
        #[cfg(unix)]
        fs::set_permissions(&dst, fs::Permissions::from_mode(AUTH_FILE_MODE)).ok();
    }
    fs::remove_dir(legacy_dir).ok();

    let plural = if count == 1 { "" } else { "s" };
    log_move("auth/", target_dir, Some(&format!("{count} file{plural}")));
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

pub fn xdg() -> Result<()> {
    let Some(legacy) = paths::legacy_home_dir() else {
        println!("Nothing to migrate. You are already using XDG directories.");
        return Ok(());
    };

    let xdg = paths::xdg_paths().context("determine XDG directories")?;

    for dir in [&xdg.state, &xdg.config, &xdg.logs] {
        fs::create_dir_all(dir).with_context(|| format!("create {}", tilde(dir)))?;
    }

    println!("Moving files from ~/.maki/ ...\n");

    move_auth(&legacy.join("auth"), &xdg.state.join("auth"))?;

    merge_dir(&legacy, &xdg.state, "sessions", false)?;
    merge_dir(&legacy, &xdg.state, "plans", false)?;
    merge_dir(&legacy, &xdg.state, "projects", true)?;
    merge_dir(&legacy, &xdg.config, "providers", false)?;

    merge_json_file(
        &legacy.join("cwd_latest.json"),
        &xdg.state.join("cwd_latest.json"),
        "cwd_latest.json",
    )?;
    merge_input_history(
        &legacy.join("input_history.json"),
        &xdg.state.join("input_history.json"),
    )?;
    merge_json_file(
        &legacy.join("model-tiers"),
        &xdg.state.join("model-tiers"),
        "model-tiers",
    )?;

    for name in ["theme", "model"] {
        let src = legacy.join(name);
        if src.exists() {
            let dst = xdg.state.join(name);
            if dst.exists() {
                fs::remove_file(&dst)
                    .with_context(|| format!("remove existing {}", tilde(&dst)))?;
            }
            move_file(&src, &dst)?;
            log_move(name, dst.parent().unwrap_or(&dst), None);
        }
    }

    move_logs(&legacy, &xdg.logs)?;

    let lock_file = legacy.join("maki.log.lock");
    if lock_file.exists() {
        fs::remove_file(&lock_file).ok();
    }

    let remaining = list_remaining(&legacy);
    let has_leftovers = !remaining.is_empty();
    let backup = legacy.with_file_name(".maki.bak");
    if has_leftovers {
        fs::create_dir_all(&backup).context("create ~/.maki.bak/")?;
        for name in &remaining {
            let src = legacy.join(name);
            let dst = backup.join(name);
            if dst.exists() {
                println!("  {name} (skipped, already in ~/.maki.bak/)");
            } else if let Err(e) = fs::rename(&src, &dst) {
                eprintln!("  warning: could not move {name} to ~/.maki.bak/: {e}");
            }
        }
    }

    if legacy.is_dir() {
        fs::remove_dir_all(&legacy).context("remove ~/.maki/")?;
    }

    println!(
        "\nAll done! Your files now live here:\n\n\
         \x20 Config   {}\n\
         \x20          init.lua, permissions.toml, mcp.toml, providers/\n\n\
         \x20 State    {}\n\
         \x20          sessions, auth, plans, memories, input history, preferences\n\n\
         \x20 Logs     {}\n\n\
         Per-project settings (.maki/ in your repos) are not affected.\n\n\
         Removed ~/.maki/.",
        tilde(&xdg.config),
        tilde(&xdg.state),
        tilde(&xdg.logs),
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
