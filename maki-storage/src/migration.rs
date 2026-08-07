//! Everything that exists only because the on-disk session shape might be
//! older than the v3 per-session folder layout (`<id>/{meta.json, log.jsonl,
//! renders.zst}`).
//!
//! Two published formats predate v3: a flat single-object `<id>.json` (v1,
//! pre-jsonl) and a flat `<id>.jsonl` (v2, the format current `main` writes:
//! header + event records with inline `out` payloads + trailing meta record).
//! Older files can also sit under a hex-UUID name from before `MakiId` went
//! base58. Migration is lazy and per-session: `sessions::load_from` parses any
//! flat file via the loaders in this module, then writes the canonical folder
//! through the normal v3 writer (`load-then-write`, no separate rewriter to
//! audit).
//!
//! Probing the on-disk shape for the picker ([`scan_entry_header`]), deleting
//! a session and its legacy siblings, and the tmp-dir swap helpers shared with
//! the v3 hot path also live here.
//!
//! The unreleased folder layout (never published) and the old `renders.bin`
//! files are deliberately not migrated: any leftover folder
//! without `meta.json` is not a session, and is invisible + unloadable.
//!
//! Divergences from the storage design doc (maki-conversation-tree-design.md,
//! author's local notes): migration keeps no `.bak` and writes no persisted
//! marker. The tmp-folder write is fsynced and swapped atomically, so folder
//! presence already proves a completed migration, and the in-process
//! `sessions::verify_folder` (record counts + renders index + header id)
//! replaces the spec's marker-based verification step; the flat file is
//! deleted immediately after the swap.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::StorageError;
use crate::id::MakiId;
use crate::session_types::{
    DEFAULT_TITLE, SessionMeta, StoredSubagent, StoredTokenUsage, normalize_title,
};
use crate::sessions::{
    LOG_FORMAT_VERSION, MAX_META_BYTES, META_FILE_NAME, SESSION_VERSION, ScannedHeader, Session,
    SessionError, SessionHeader, lock_session, read_capped,
};

pub(crate) const TMP_DIR_SUFFIX: &str = ".tmp";
pub(crate) const OLD_DIR_SUFFIX: &str = ".old";

// -- Path probe + legacy cleanup --

fn is_jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

/// All non-canonical files under `sessions/` whose stem parses back to `id`
/// (e.g. a hex-UUID filename alongside the base58 folder). Excludes the
/// canonical base58 path itself; the caller deletes the folder separately.
fn find_legacy_files(dir: &Path, id: MakiId) -> Vec<PathBuf> {
    let canonical = id.to_string();
    crate::sessions::session_entries(dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| {
            p.extension().is_none_or(|e| e != "lock")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s != canonical && s.parse::<MakiId>() == Ok(id))
        })
        .collect()
}

/// Pick the flat file to load from when no folder exists: prefer `.jsonl`
/// (newer format) over `.json`, and consider legacy-hex-id siblings.
pub(crate) fn legacy_flat_file(dir: &Path, id: MakiId) -> Option<PathBuf> {
    for ext in ["jsonl", "json"] {
        let path = dir.join(format!("{id}.{ext}"));
        if path.exists() {
            return Some(path);
        }
    }
    let legacy = find_legacy_files(dir, id);
    legacy
        .iter()
        .find(|p| is_jsonl(p))
        .or_else(|| legacy.first())
        .cloned()
}

/// Remove any flat `<id>.json`, `<id>.jsonl`, and legacy-hex-id siblings of
/// `id` left behind after the canonical folder exists. Idempotent. Returns
/// whether anything was removed.
pub(crate) fn remove_legacy_files(dir: &Path, id: MakiId) -> Result<bool, SessionError> {
    let mut removed = try_remove(&dir.join(format!("{id}.json")))?;
    removed |= try_remove(&dir.join(format!("{id}.jsonl")))?;
    for legacy in find_legacy_files(dir, id) {
        removed |= try_remove(&legacy)?;
    }
    Ok(removed)
}

fn try_remove(path: &Path) -> Result<bool, StorageError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn try_remove_dir_all(path: &Path) -> Result<bool, StorageError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

// -- Folder swap helpers (shared with the v3 hot path) --

pub(crate) fn tmp_sibling(folder: &Path) -> PathBuf {
    let name = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    folder.with_file_name(format!("{name}{TMP_DIR_SUFFIX}"))
}

pub(crate) fn old_sibling(folder: &Path) -> PathBuf {
    let name = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    folder.with_file_name(format!("{name}{OLD_DIR_SUFFIX}"))
}

/// Swap a freshly written `tmp` directory into `folder`. If `folder` already
/// exists (a rewrite over a live session), `rename` cannot replace a non-empty
/// directory, so it moves the old folder aside to `<id>.old` first, renames
/// `tmp` into place, then deletes the stale copy. A crash anywhere leaves the
/// session recoverable in either `.tmp` or `.old` instead of deleted.
pub(crate) fn persist_folder(tmp: &Path, folder: &Path) -> Result<(), StorageError> {
    if !folder.exists() {
        return Ok(fs::rename(tmp, folder)?);
    }
    let old = old_sibling(folder);
    let _ = fs::remove_dir_all(&old).ok();
    fs::rename(folder, &old)?;
    fs::rename(tmp, folder)?;
    // Make the swap durable before the stale copy is unlinked, so a crash
    // cannot lose both the new folder and the recovery copy on filesystems
    // without ordered journaling.
    fsync_dir(folder.parent().unwrap_or(folder))?;
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

/// Remove leftover folder-swap debris: any `<id>.tmp` dir outright, and any
/// `<id>.old` dir once its sibling folder exists again. A crash in
/// [`persist_folder`] leaves the session recoverable in either place.
pub(crate) fn sweep_stray_dirs(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let (suffix, sweep_when_sibling) = if name.ends_with(TMP_DIR_SUFFIX) {
            (TMP_DIR_SUFFIX, true)
        } else if name.ends_with(OLD_DIR_SUFFIX) {
            (OLD_DIR_SUFFIX, false)
        } else {
            continue;
        };
        let id_name = &name[..name.len() - suffix.len()];
        let Ok(id) = id_name.parse::<MakiId>() else {
            continue;
        };
        // A live writer owns `.tmp` mid-swap (and `.old` between the
        // renames); only sweep debris whose session lock is free. The lock
        // stays held through the removal so a rewrite cannot start for the
        // same id mid-sweep.
        let Ok(_lock) = lock_session(dir, id) else {
            continue;
        };
        let remove = sweep_when_sibling || path.with_file_name(id_name).is_dir();
        if remove {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

#[cfg(unix)]
pub(crate) fn fsync_dir(dir: &Path) -> Result<(), StorageError> {
    let f = fs::File::open(dir)?;
    f.sync_data()?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn fsync_dir(_dir: &Path) -> Result<(), StorageError> {
    Ok(())
}

// -- Scan record types --

/// Identity + summary of a flat single-object `.json` session (v1), enough
/// for the picker; the full parse is reserved for when the session opens.
#[derive(Deserialize)]
struct LegacyHeader {
    version: u32,
    id: MakiId,
    title: String,
    cwd: String,
    updated_at: u64,
}

/// First-line header of a flat v2 `.jsonl` file. Only the fields the picker
/// needs are decoded; the full record is re-parsed at load time.
#[derive(Deserialize)]
struct JsonlHeader {
    v: u32,
    id: MakiId,
    cwd: String,
}

/// Tag-only probe of a `log.jsonl` tail line: distinguishes the last `meta`
/// record (fresh title + `updated_at`) from any other record type.
#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ScanRecord {
    Meta {
        title: String,
        updated_at: u64,
    },
    #[serde(other)]
    Other,
}

// -- Picker scan: extract identity + summary from any on-disk shape --

const TAIL_BUF: u64 = 4096;
const SCAN_LEGACY_BYTES: u64 = 64 * 1024 * 1024;
const SCAN_LINE_BYTES: u64 = 1024 * 1024;
const MIN_JSONL_VERSION: u32 = 2;
const MAX_LEGACY_BYTES: u64 = 1024 * 1024 * 1024;

/// v3 folder scan: read `meta.json` directly. One file open + one parse, no
/// tail scan of `log.jsonl`. A folder without `meta.json` is not a session.
fn scan_meta_header(folder: &Path) -> Option<ScannedHeader> {
    let path = folder.join(META_FILE_NAME);
    let len = fs::metadata(&path).ok()?.len();
    if len > MAX_META_BYTES {
        return None;
    }
    let data = fs::read(path).ok()?;
    let header: SessionHeader<serde_json::Value> = serde_json::from_slice(&data).ok()?;
    Some(ScannedHeader {
        id: header.id,
        cwd: header.cwd,
        title: header.title,
        updated_at: header.updated_at,
    })
}

/// Scan a flat v2 `.jsonl` file (header line + events + trailing meta line).
/// Reads just the first line for identity, then tail-seeks for the last
/// `meta` record to recover `title` + `updated_at`. `v` bounded by the range
/// of published listable versions.
fn scan_jsonl_header(path: &Path) -> Option<ScannedHeader> {
    let mut file = File::open(path).ok()?;
    let header: JsonlHeader = {
        let mut reader = BufReader::new(file.by_ref().take(SCAN_LINE_BYTES));
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        serde_json::from_str(line.trim_end()).ok()?
    };
    if header.v > LOG_FORMAT_VERSION || header.v < MIN_JSONL_VERSION {
        return None;
    }

    let (title, updated_at) =
        read_last_meta(&mut file).unwrap_or_else(|| (DEFAULT_TITLE.to_string(), 0));

    Some(ScannedHeader {
        id: header.id,
        cwd: header.cwd,
        title,
        updated_at,
    })
}

/// Tail-scan for the last `meta` record so the picker can show the latest
/// title + `updated_at`. Doubles the read window until a complete final line
/// is visible, bounded by the file length.
fn read_last_meta(file: &mut File) -> Option<(String, u64)> {
    let len = file.seek(SeekFrom::End(0)).ok()?;
    let mut tail = TAIL_BUF.min(len);
    loop {
        file.seek(SeekFrom::End(-(tail as i64))).ok()?;
        let mut buf = vec![0u8; tail as usize];
        file.read_exact(&mut buf).ok()?;

        let content = buf.strip_suffix(b"\n").unwrap_or(&buf);
        if let Some(nl) = content.iter().rposition(|&b| b == b'\n') {
            let last_line = &content[nl + 1..];
            if let Ok(ScanRecord::Meta { title, updated_at }) = serde_json::from_slice(last_line) {
                return Some((title, updated_at));
            }
            return None;
        }

        if tail >= len {
            return None;
        }
        tail = (tail * 2).min(len);
    }
}

/// Scan a flat single-object `.json` session (v1): one read + one parse.
fn scan_legacy_header(path: &Path) -> Option<ScannedHeader> {
    let len = fs::metadata(path).ok()?.len();
    if len > SCAN_LEGACY_BYTES {
        return None;
    }
    let data = fs::read(path).ok()?;
    let h: LegacyHeader = serde_json::from_slice(&data).ok()?;
    if h.version != SESSION_VERSION {
        return None;
    }
    Some(ScannedHeader {
        id: h.id,
        cwd: h.cwd,
        title: h.title,
        updated_at: h.updated_at,
    })
}

/// Unified entry point for `sessions::scan_headers`. Given a directory entry
/// under `sessions/`, return its identity + latest summary if it belongs to a
/// session of any published layout (v3 folder, flat `.jsonl`, flat `.json`),
/// else `None` (so the picker skips it without re-reading every list).
pub(crate) fn scan_entry_header(path: &Path) -> Option<ScannedHeader> {
    if path.is_dir() {
        scan_meta_header(path)
    } else if is_jsonl(path) {
        scan_jsonl_header(path)
    } else {
        scan_legacy_header(path)
    }
}

// -- Legacy loaders (published flat formats, load-then-write) --
// -- Legacy loading (published flat formats, load-then-write) --

/// Full record set of the published flat `.jsonl` format (v2): header +
/// events with `out` payloads inline + trailing meta record. The v3
/// `log.jsonl` is a pure event stream and never contains these; only the
/// legacy loader parses them.
#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
enum LegacyRecord<M, U, T> {
    #[serde(rename = "header")]
    Header {
        v: u32,
        id: MakiId,
        model: String,
        cwd: String,
        created_at: u64,
    },
    #[serde(rename = "msg")]
    Msg { d: M },
    #[serde(rename = "out")]
    Out { id: String, d: T },
    #[serde(rename = "sub_msg")]
    SubMsg { sub: String, d: M },
    #[serde(rename = "meta")]
    Meta {
        title: String,
        token_usage: U,
        updated_at: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        subagents: Vec<StoredSubagent>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        usage_by_model: HashMap<String, StoredTokenUsage>,
        #[serde(flatten)]
        meta: SessionMeta,
    },
}

/// Tag-only probe that classifies a line that failed the strict
/// [`LegacyRecord`] parse: distinguishes a header with a bad id from a
/// genuinely unknown record.
#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum RawTag {
    Header {
        id: String,
    },
    #[serde(other)]
    Other,
}

/// Parse a flat legacy session file into a `Session`: a v2 `.jsonl` via
/// [`load_jsonl`], a v1 single-object `.json` via a direct deserialize.
/// `load_from` writes the canonical v3 folder from the result.
pub(crate) fn load_session_at<M, U, T>(path: &Path) -> Result<Session<M, U, T>, SessionError>
where
    M: DeserializeOwned,
    U: DeserializeOwned + Default,
    T: DeserializeOwned,
{
    let data = read_capped(path, MAX_LEGACY_BYTES)?;
    let mut session: Session<M, U, T> = if path.extension().is_some_and(|e| e == "jsonl") {
        load_jsonl(&data, &path.display().to_string())?
    } else {
        let session: Session<M, U, T> =
            serde_json::from_slice(&data).map_err(StorageError::from)?;
        if session.version != SESSION_VERSION {
            return Err(SessionError::VersionMismatch {
                found: session.version,
                expected: SESSION_VERSION,
            });
        }
        session
    };
    session.title = normalize_title(&session.title);
    Ok(session)
}

/// Parse a flat v2 `.jsonl` byte slice into a `Session`. Header + events
/// carry the conversation; the last `meta` record wins for summary fields.
/// Any trailing unparseable lines (a partial flush) are logged + skipped.
fn load_jsonl<M, U, T>(data: &[u8], display_path: &str) -> Result<Session<M, U, T>, SessionError>
where
    M: DeserializeOwned,
    U: DeserializeOwned + Default,
    T: DeserializeOwned,
{
    let mut line_count = 0usize;

    let mut id: Option<MakiId> = None;
    let mut model = String::new();
    let mut cwd = String::new();
    let mut created_at = 0u64;
    let mut messages: Vec<M> = Vec::new();
    let mut tool_outputs = HashMap::new();
    let mut subagent_messages: HashMap<String, Vec<M>> = HashMap::new();
    let mut title = DEFAULT_TITLE.to_string();
    let mut token_usage = U::default();
    let mut updated_at = 0u64;
    let mut subagents = Vec::new();
    let mut usage_by_model = HashMap::new();
    let mut meta = SessionMeta::default();
    let mut got_header = false;

    for line in data.split(|&b| b == b'\n') {
        line_count += 1;
        if line.is_empty() {
            continue;
        }
        let record: LegacyRecord<M, U, T> = match serde_json::from_slice(line) {
            Ok(r) => r,
            Err(e) => {
                if !got_header
                    && let Ok(RawTag::Header { id: raw_id }) = serde_json::from_slice(line)
                    && let Err(source) = raw_id.parse::<MakiId>()
                {
                    return Err(SessionError::CorruptHeaderId {
                        path: display_path.to_string(),
                        raw_id,
                        source,
                    });
                }
                warn!(
                    path = display_path,
                    error = %e,
                    line = line_count,
                    "skipping unrecognized JSONL record",
                );
                continue;
            }
        };
        match record {
            LegacyRecord::Header {
                v,
                id: h_id,
                model: h_model,
                cwd: h_cwd,
                created_at: h_created,
            } => {
                if !(MIN_JSONL_VERSION..=LOG_FORMAT_VERSION).contains(&v) {
                    return Err(SessionError::VersionMismatch {
                        found: v,
                        expected: LOG_FORMAT_VERSION,
                    });
                }
                id = Some(h_id);
                model = h_model;
                cwd = h_cwd;
                created_at = h_created;
                got_header = true;
            }
            LegacyRecord::Msg { d } => messages.push(d),
            LegacyRecord::Out { id: out_id, d } => {
                tool_outputs.insert(out_id, Arc::new(d));
            }
            LegacyRecord::SubMsg { sub, d } => {
                subagent_messages.entry(sub).or_default().push(d);
            }
            LegacyRecord::Meta {
                title: m_title,
                token_usage: m_usage,
                updated_at: m_updated,
                subagents: m_subagents,
                usage_by_model: m_usage_by_model,
                meta: m_meta,
            } => {
                title = m_title;
                token_usage = m_usage;
                updated_at = m_updated;
                subagents = m_subagents;
                usage_by_model = m_usage_by_model;
                meta = m_meta;
            }
        }
    }

    let id = id.ok_or(StorageError::NotFound(display_path.to_string()))?;

    Ok(Session::from_parts(
        SessionHeader {
            log_format_version: LOG_FORMAT_VERSION,
            id,
            created_at,
            updated_at,
            model,
            cwd,
            title,
            token_usage,
            subagents,
            usage_by_model,
            meta,
            parent_session_id: None,
            created_from_node_id: None,
        },
        messages,
        tool_outputs,
        subagent_messages,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tempfile::TempDir;

    fn user_message(text: &str) -> Value {
        serde_json::json!({"role":"user","content":[{"type":"text","text":text}]})
    }

    fn legacy_jsonl_v2(id: MakiId) -> String {
        let mut buf = serde_json::to_vec(&serde_json::json!({
            "t":"header","v":2,"id":id.to_string(),"model":"m","cwd":"/project","created_at":0
        }))
        .unwrap();
        buf.push(b'\n');
        buf.extend(
            serde_json::to_vec(&serde_json::json!({"t":"msg","d":user_message("hello")})).unwrap(),
        );
        buf.push(b'\n');
        buf.extend(
            serde_json::to_vec(&serde_json::json!({"t":"out","id":"toolu_1","d":{"v":1}})).unwrap(),
        );
        buf.push(b'\n');
        buf.extend(
            serde_json::to_vec(&serde_json::json!({
                "t":"meta","title":"t","token_usage":null,"updated_at":0
            }))
            .unwrap(),
        );
        buf.push(b'\n');
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn hex_id_sibling_is_found_and_removed() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let id: MakiId = "019650874c717f008000000000000000".parse().unwrap();
        let legacy_path = dir.join("01965087-4c71-7f00-8000-000000000000.jsonl");
        fs::write(&legacy_path, legacy_jsonl_v2(id)).unwrap();

        assert_eq!(legacy_flat_file(dir, id), Some(legacy_path.clone()));
        assert!(remove_legacy_files(dir, id).unwrap());
        assert!(!legacy_path.exists());
    }

    #[test]
    fn lock_file_sibling_is_not_removed() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let id: MakiId = "019650874c717f008000000000000000".parse().unwrap();
        let lock_path = dir.join(format!("{id}.lock"));
        fs::write(&lock_path, "").unwrap();
        fs::write(dir.join(format!("{id}.jsonl")), legacy_jsonl_v2(id)).unwrap();

        assert!(remove_legacy_files(dir, id).unwrap());
        assert!(!dir.join(format!("{id}.jsonl")).exists());
        assert!(lock_path.exists());
    }

    #[test]
    fn scan_entry_header_understands_every_published_shape() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let id: MakiId = "019650874c717f008000000000000000".parse().unwrap();

        let flat = dir.join(format!("{id}.jsonl"));
        fs::write(&flat, legacy_jsonl_v2(id)).unwrap();
        let scanned = scan_entry_header(&flat).unwrap();
        assert_eq!(scanned.id, id);
        assert_eq!(scanned.title, "t");

        let v1 = dir.join("some-json.json");
        fs::write(
            &v1,
            serde_json::to_vec(&serde_json::json!({
                "version": SESSION_VERSION,
                "id": id,
                "title": "old",
                "cwd": "/x",
                "updated_at": 42,
            }))
            .unwrap(),
        )
        .unwrap();
        let scanned = scan_entry_header(&v1).unwrap();
        assert_eq!(scanned.title, "old");
        assert_eq!(scanned.updated_at, 42);
    }
}
