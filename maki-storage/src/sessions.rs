//! Session persistence in the v3 per-session folder layout
//! (`<id>/{meta.json, log.jsonl, renders.zst}`).
//!
//! `meta.json` carries identity + summary and is replaced atomically on every
//! change; `log.jsonl` is a pure append-only event stream (`msg` / `out` marker /
//! `sub_msg`) whose bytes never shift once written; `renders.zst` is an
//! append-only zstd frame store keyed by tool_use_id holding the `out` payloads.
//!
//! `SessionLog` tracks cursor state for O(delta) incremental saves and mints a
//! session `epoch` on every structural change; an `append` whose epoch no longer
//! matches must be rewritten, which is how rewind, subagent replacement and
//! truncation stay sound. The serializable content model (settings, effort,
//! thinking, titles) lives in [`crate::session_types`]. Everything that exists
//! only because the on-disk shape might be older (flat `.json` / `.jsonl`,
//! hex-id siblings) lives in [`crate::migration`].

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::id::{MakiId, MakiIdParseError, MessageId};
use crate::migration::{
    OLD_DIR_SUFFIX, TMP_DIR_SUFFIX, fsync_dir, legacy_flat_file, load_session_at, old_sibling,
    persist_folder, remove_legacy_files, scan_entry_header, sweep_stray_dirs, tmp_sibling,
    try_remove_dir_all,
};
use crate::renders::{RenderError, RenderStore};
use crate::session_types::{
    DEFAULT_TITLE, SessionMeta, StoredMode, StoredSubagent, StoredTokenUsage, TitleSource,
    generate_title, normalize_title,
};
use crate::{StateDir, StorageError, atomic_write, now_epoch};

pub(crate) const SESSION_VERSION: u32 = 1;
pub const LOG_FORMAT_VERSION: u32 = 3;
pub const SESSIONS_DIR: &str = "sessions";
pub const LOG_FILE_NAME: &str = "log.jsonl";
pub const META_FILE_NAME: &str = "meta.json";
pub(crate) const MAX_META_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LOG_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_TOTAL_TOOL_OUTPUT_BYTES: usize = 1024 * 1024 * 1024;
const CWD_INDEX_FILE: &str = "cwd_latest.json";
const CWD_INDEX_STEM: &str = "cwd_latest";
const SCAN_CACHE_FILE: &str = "scan_cache.json";
const SCAN_CACHE_STEM: &str = "scan_cache";
const NON_SESSION_STEMS: [&str; 2] = [CWD_INDEX_STEM, SCAN_CACHE_STEM];
const EPOCH_CHANGED: &str = "messages were rewritten";
const FILE_CHANGED_UNDERNEATH: &str = "file changed underneath";
const CURSOR_AHEAD: &str = "cursor ahead of session";

/// Hands out the token that tags one append-only run of a message list.
/// Process wide, so two runs never pick the same number.
static EPOCH: AtomicU64 = AtomicU64::new(1);

pub fn next_epoch() -> u64 {
    EPOCH.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("incompatible session version {found} (expected {expected})")]
    VersionMismatch { found: u32, expected: u32 },
    #[error("session ID mismatch: log owns {log_id}, got {given_id}")]
    IdMismatch { log_id: MakiId, given_id: MakiId },
    #[error("session log {path} has header id {raw_id:?} that is not a valid id: {source}")]
    CorruptHeaderId {
        path: String,
        raw_id: String,
        source: MakiIdParseError,
    },
    #[error("session log diverged ({reason}); rewrite required")]
    LogDiverged { reason: &'static str },
    #[error(transparent)]
    Render(#[from] RenderError),
    #[error("session {id} is locked by another process")]
    Locked { id: MakiId },
    #[error("meta.json in {folder} claims id {claimed}, but the folder is named {folder_name}")]
    HeaderIdMismatch {
        folder: String,
        claimed: MakiId,
        folder_name: String,
    },
    #[error("folder verification failed: {0}")]
    Verify(String),
}

/// Messages plus the token of the run they belong to. Comparing tokens tells
/// an append from a rewrite, with no need to diff the lists.
#[derive(Clone)]
pub struct HistorySnapshot<M> {
    pub epoch: u64,
    pub messages: Arc<Vec<M>>,
}

impl<M> HistorySnapshot<M> {
    pub fn new(messages: Vec<M>) -> Self {
        Self {
            epoch: next_epoch(),
            messages: Arc::new(messages),
        }
    }
}

impl<M> Default for HistorySnapshot<M> {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// The conversation collections are private so every change goes through a
/// mutator that classifies itself: `revision` says "this needs writing",
/// `epoch` says "append cursors into the log are void". The other fields stay
/// public because the meta record is rewritten in full on every append, so
/// they hold no cursor to spoil. `cwd` and `model` are the exception: they
/// live in the header record, which only a rewrite touches, so changes must
/// go through their setters.
///
/// [`SessionMeta`] is the part the owner mirrors from its own live state and
/// hands over whole on every checkpoint. Whatever the session maintains itself
/// gets a field of its own instead, so a checkpoint never copies it out and
/// back in only to compare it against itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session<M, U, T> {
    pub version: u32,
    pub id: MakiId,
    pub title: String,
    pub cwd: String,
    pub model: String,
    messages: Arc<Vec<M>>,
    pub token_usage: U,
    #[serde(default = "HashMap::new")]
    tool_outputs: HashMap<String, Arc<T>>,
    #[serde(default = "HashMap::new", skip_serializing_if = "HashMap::is_empty")]
    subagent_messages: HashMap<String, Arc<Vec<M>>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    subagents: Vec<StoredSubagent>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    usage_by_model: HashMap<String, StoredTokenUsage>,
    #[serde(flatten)]
    pub meta: SessionMeta,
    pub created_at: u64,
    pub updated_at: u64,
    /// Bumped by every mutation, so a checkpoint knows if there is anything
    /// to write.
    #[serde(skip)]
    revision: u64,
    /// Bumped by every mutation except `meta`, so a checkpoint can tell a tool
    /// result, which has to reach disk now, from a keystroke in the draft,
    /// which can wait for the keystrokes behind it.
    #[serde(skip)]
    content_revision: u64,
    /// The append-only run `messages` belongs to, adopted from the producer's
    /// snapshot or minted fresh when this session rewrites them itself. Once
    /// it changes, every append cursor into the log is void.
    #[serde(skip, default = "next_epoch")]
    epoch: u64,
}

#[derive(Serialize)]
pub struct SessionSummary {
    pub id: MakiId,
    pub title: String,
    pub updated_at: u64,
}

// -- Session metadata header (meta.json) --

/// Per-session `meta.json` payload: identity + summary fields that mutate
/// without touching the event log. Replaced atomically on every save that
/// changes any field here; `log.jsonl` stays append-only and never shifts
/// bytes for a header change. `subagents` / `usage_by_model` live on the
/// `Session` struct, so the header carries them alongside the flattened
/// `SessionMeta`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader<U> {
    pub log_format_version: u32,
    pub id: MakiId,
    pub created_at: u64,
    pub updated_at: u64,
    pub model: String,
    pub cwd: String,
    pub title: String,
    pub token_usage: U,
    #[serde(default)]
    pub subagents: Vec<StoredSubagent>,
    #[serde(default)]
    pub usage_by_model: HashMap<String, StoredTokenUsage>,
    #[serde(default)]
    pub meta: SessionMeta,
    /// Fork lineage (PR-5 populates these; the wire shape is fixed now so a
    /// fork never needs a format change).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_from_node_id: Option<String>,
}

/// Borrowed serialization twin of [`SessionHeader`] so [`SessionLog::append`]
/// can serialize without cloning `U` / `SessionMeta` on the hot save path.
#[derive(Serialize)]
struct SessionHeaderRef<'a, U: Serialize> {
    log_format_version: u32,
    id: MakiId,
    created_at: u64,
    updated_at: u64,
    model: &'a str,
    cwd: &'a str,
    title: &'a str,
    token_usage: &'a U,
    subagents: &'a [StoredSubagent],
    usage_by_model: &'a HashMap<String, StoredTokenUsage>,
    meta: &'a SessionMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_from_node_id: Option<&'a str>,
}

fn header_ref<M, U, T>(session: &Session<M, U, T>) -> SessionHeaderRef<'_, U>
where
    U: Serialize,
{
    SessionHeaderRef {
        log_format_version: LOG_FORMAT_VERSION,
        id: session.id,
        created_at: session.created_at,
        updated_at: session.updated_at,
        model: &session.model,
        cwd: &session.cwd,
        title: &session.title,
        token_usage: &session.token_usage,
        subagents: &session.subagents,
        usage_by_model: &session.usage_by_model,
        meta: &session.meta,
        parent_session_id: None,
        created_from_node_id: None,
    }
}

fn serialize_header<M, U, T>(session: &Session<M, U, T>) -> Result<Vec<u8>, SessionError>
where
    U: Serialize,
{
    Ok(serde_json::to_vec_pretty(&header_ref(session)).map_err(StorageError::from)?)
}

// -- JSONL event records --

/// Pure append-only event records in `log.jsonl`. The session header lives in
/// `meta.json`; `Out` is a marker whose payload is keyed by `id` in
/// `renders.zst`. Anything else on disk is a legacy shape parsed in
/// [`crate::migration`].
#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum LogRecord<M> {
    #[serde(rename = "msg")]
    Msg {
        id: MessageId,
        parent_id: Option<MessageId>,
        timestamp: u64,
        d: M,
    },
    #[serde(rename = "out")]
    Out { id: String },
    #[serde(rename = "sub_msg")]
    SubMsg { sub: String, d: M },
}

// -- SessionLog: append-only persistence --

/// Lock-holding append cursor over one session's `log.jsonl`: `append` writes
/// only the delta since the last write and rolls the file back on failure.
pub struct SessionLog {
    session_id: MakiId,
    folder: PathBuf,
    file: File,
    renders: RenderStore,
    /// Held for this cursor's lifetime; drop releases the flock on `<id>.lock`.
    _lock: File,
    /// The session's `epoch` at the last write. Appending is sound only while
    /// it stays the same.
    saved_epoch: u64,
    /// Length of `log.jsonl` after the last write. Anything else means someone
    /// truncated, deleted or wrote it, and an append would corrupt it.
    saved_len: u64,
    saved_msg_count: usize,
    saved_tool_ids: HashSet<String>,
    saved_sub_msg_counts: HashMap<String, usize>,
    /// Id of the last `msg` record on disk: the parent the next appended
    /// message chains onto.
    last_msg_id: Option<MessageId>,
    /// Last bytes written to `meta.json`, or `None` if mutated in-memory but
    /// not yet persisted. Lets `append` skip rewriting `meta.json` when only
    /// event state changed, and skip event writes when only meta changed.
    saved_meta: Option<Vec<u8>>,
}

fn sub_msg_snapshot<M>(map: &HashMap<String, Arc<Vec<M>>>) -> HashMap<String, usize> {
    map.iter().map(|(k, v)| (k.clone(), v.len())).collect()
}

impl SessionLog {
    /// Writes the whole session through the atomic folder swap, so a crash
    /// mid-write leaves the old folder intact, then claims the cwd index and
    /// sweeps legacy leftovers. The only way to get a usable cursor onto a
    /// folder this process did not write: a cursor read back from disk
    /// describes the session that was loaded, never the live one.
    pub fn rewrite<M, U, T>(dir: &Path, session: &Session<M, U, T>) -> Result<Self, SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        fs::create_dir_all(dir).map_err(StorageError::from)?;
        let lock = lock_session(dir, session.id)?;
        let log = Self::write_canonical(dir, session, lock)?;
        update_cwd_index(dir, &session.cwd, session.id)?;
        Ok(log)
    }

    /// [`Self::rewrite`] without claiming the cwd index: migrating a legacy
    /// file on load must not make that session the cwd's latest.
    fn write_canonical<M, U, T>(
        dir: &Path,
        session: &Session<M, U, T>,
        lock: File,
    ) -> Result<Self, SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        let (folder, file, renders, last_msg_id) = write_folder_atomic(dir, session)?;
        if let Err(e) = remove_legacy_files(dir, session.id) {
            warn!(error = %e, "legacy session files remain after rewrite");
        }
        Ok(Self::cursor_from(
            session,
            folder,
            file,
            renders,
            lock,
            last_msg_id,
        ))
    }

    pub fn session_id(&self) -> MakiId {
        self.session_id
    }

    /// Cursor onto the folder of an already-loaded `session`, without re-reading
    /// it from disk. Sound only because `session` was just loaded from that
    /// folder: the cursors (epoch, counts, file length) describe exactly what
    /// is on disk, so the first append is a clean delta instead of a rewrite.
    pub fn open<M, U, T>(dir: &Path, session: &Session<M, U, T>) -> Result<Self, SessionError>
    where
        U: Serialize,
    {
        let lock = lock_session(dir, session.id)?;
        let folder = session_dir(dir, session.id);
        let path = folder.join(LOG_FILE_NAME);
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(StorageError::from)?;
        let renders = RenderStore::open(&folder)?;
        let last_msg_id = last_msg_id(&folder);
        Ok(Self::cursor_from(
            session,
            folder,
            file,
            renders,
            lock,
            last_msg_id,
        ))
    }

    pub fn append<M, U, T>(&mut self, session: &Session<M, U, T>) -> Result<(), SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        self.require_same_id(session)?;
        self.ensure_appendable(session)?;

        let mut event_buf = Vec::new();
        let mut new_msg_count = self.saved_msg_count;
        let mut new_tool_ids: Vec<(&String, &Arc<T>)> = Vec::new();

        let mut next_msg_id = self.last_msg_id;
        for msg in &session.messages[self.saved_msg_count..] {
            let id = MessageId::generate();
            append_record(
                &mut event_buf,
                &LogRecord::<&M>::Msg {
                    id,
                    parent_id: next_msg_id,
                    timestamp: now_epoch(),
                    d: msg,
                },
            )?;
            next_msg_id = Some(id);
            new_msg_count += 1;
        }

        for (id, output) in &session.tool_outputs {
            if !self.saved_tool_ids.contains(id) {
                append_record(&mut event_buf, &LogRecord::<&M>::Out { id: id.clone() })?;
                new_tool_ids.push((id, output));
            }
        }

        let mut new_sub_counts: Vec<(String, usize)> = Vec::new();
        for (sub_id, msgs) in &session.subagent_messages {
            let saved = self.saved_sub_msg_counts.get(sub_id).copied().unwrap_or(0);
            for msg in &msgs[saved..] {
                append_record(
                    &mut event_buf,
                    &LogRecord::<&M>::SubMsg {
                        sub: sub_id.clone(),
                        d: msg,
                    },
                )?;
            }
            if msgs.len() > saved {
                new_sub_counts.push((sub_id.clone(), msgs.len()));
            }
        }

        let meta_bytes = serialize_header(session)?;
        let meta_unchanged = self.saved_meta.as_deref() == Some(meta_bytes.as_slice());

        if event_buf.is_empty() && meta_unchanged && new_tool_ids.is_empty() {
            return Ok(());
        }

        // Renders land first: an `out` marker must never point at a frame a
        // crash left unwritten. A crash between renders and log leaves orphan
        // frames no marker references (harmless).
        let render_start = self.renders.writer_len()?;
        for (id, output) in &new_tool_ids {
            if let Err(e) = self.renders.append(id, output.as_ref()) {
                self.rollback_renders(render_start);
                return Err(e.into());
            }
        }

        // Meta (atomic rename) lands before events so a torn log write cannot
        // strand event bytes the header never describes. A crash mid-meta
        // rename leaves either the prior or the new `meta.json`, both valid;
        // a failed write rolls back renders and aborts before any event bytes
        // are appended, so a retry re-encodes cleanly.
        if !meta_unchanged {
            self.write_meta(&meta_bytes, render_start)?;
            self.saved_meta = Some(meta_bytes);
        }

        // Event log is the commit fence: a successful fsync means renders +
        // meta + events are all durable. A failed write rolls back renders
        // and truncates the log to the pre-append boundary so a retry
        // re-encodes cleanly instead of duplicating events. `meta.json` may
        // already carry the new state; that lag is cosmetic since msg/tool
        // counts re-derive from `log.jsonl` on load.
        let log_start = self.file.metadata().map_err(StorageError::from)?.len();
        if let Err(e) = self
            .file
            .write_all(&event_buf)
            .and_then(|()| self.file.sync_data())
        {
            let _ = self.file.set_len(log_start);
            self.rollback_renders(render_start);
            return Err(StorageError::from(e).into());
        }

        self.saved_len += event_buf.len() as u64;
        self.saved_msg_count = new_msg_count;
        self.last_msg_id = next_msg_id;
        self.saved_tool_ids
            .extend(new_tool_ids.iter().map(|(id, _)| (*id).clone()));
        for (sub_id, count) in new_sub_counts {
            self.saved_sub_msg_counts.insert(sub_id, count);
        }

        Ok(())
    }

    fn cursor_from<M, U, T>(
        session: &Session<M, U, T>,
        folder: PathBuf,
        file: File,
        renders: RenderStore,
        lock: File,
        last_msg_id: Option<MessageId>,
    ) -> Self
    where
        U: Serialize,
    {
        let saved_len = file.metadata().map(|m| m.len()).unwrap_or_default();
        Self {
            session_id: session.id,
            folder,
            file,
            renders,
            _lock: lock,
            saved_epoch: session.epoch,
            saved_len,
            saved_msg_count: session.messages.len(),
            last_msg_id,
            saved_tool_ids: session.tool_outputs.keys().cloned().collect(),
            saved_sub_msg_counts: sub_msg_snapshot(&session.subagent_messages),
            saved_meta: serialize_header(session).ok(),
        }
    }

    fn require_same_id<M, U, T>(&self, session: &Session<M, U, T>) -> Result<(), SessionError> {
        if session.id != self.session_id {
            return Err(SessionError::IdMismatch {
                log_id: self.session_id,
                given_id: session.id,
            });
        }
        Ok(())
    }

    /// Ok only while every cursor still describes the file, which is what makes
    /// `saved_len` the file length and the rest of the cursors its content, and
    /// while appending is still cheaper than starting the file over.
    fn ensure_appendable<M, U, T>(&self, session: &Session<M, U, T>) -> Result<(), SessionError> {
        let reason = if session.epoch != self.saved_epoch {
            EPOCH_CHANGED
        } else if self.file.metadata().map_err(StorageError::from)?.len() != self.saved_len {
            FILE_CHANGED_UNDERNEATH
        } else if self.cursor_ahead(session) {
            // Nothing shrinks a session without minting a new epoch, so this
            // should never fire. It stays because the slices in `append` would
            // panic instead of corrupting if it ever does.
            CURSOR_AHEAD
        } else {
            return Ok(());
        };
        Err(SessionError::LogDiverged { reason })
    }

    fn cursor_ahead<M, U, T>(&self, session: &Session<M, U, T>) -> bool {
        self.saved_msg_count > session.messages.len()
            || self
                .saved_tool_ids
                .iter()
                .any(|id| !session.tool_outputs.contains_key(id))
            || self.saved_sub_msg_counts.iter().any(|(sub, &count)| {
                session
                    .subagent_messages
                    .get(sub)
                    .is_none_or(|msgs| count > msgs.len())
            })
    }

    /// Atomic-replace `meta.json` with `bytes`. The fsync fence keeps the
    /// rename durable before events write.
    fn write_meta(&mut self, bytes: &[u8], render_start: u64) -> Result<(), SessionError> {
        let meta_path = self.folder.join(META_FILE_NAME);
        atomic_write(&meta_path, bytes)?;
        if let Err(e) = fsync_dir(&self.folder) {
            self.rollback_renders(render_start);
            return Err(e.into());
        }
        Ok(())
    }

    /// Truncate `renders.zst` back to `render_start` (`set_len`) and rebuild
    /// the in-memory index from disk so the next append resumes cleanly
    /// instead of re-writing already-rolled-back frames.
    fn rollback_renders(&mut self, render_start: u64) {
        let _ = self.renders.truncate_writer(render_start);
        let Some(folder) = self.renders.path().parent().map(Path::to_path_buf) else {
            return;
        };
        match RenderStore::open(&folder) {
            Ok(r) => self.renders = r,
            Err(e) => warn!(error = %e, "failed to re-open renders store after rollback"),
        }
    }
}

fn append_record<R: Serialize>(buf: &mut Vec<u8>, record: &R) -> Result<(), SessionError> {
    serde_json::to_writer(&mut *buf, record).map_err(StorageError::from)?;
    buf.push(b'\n');
    Ok(())
}

/// Atomically write the canonical `<id>/` folder (`meta.json`, `log.jsonl`,
/// `renders.zst`) into `<id>.tmp/`, fsync, verify the tmp folder against the
/// in-memory session, then swap into place. Returns the folder path, an open
/// append handle on `log.jsonl`, a fresh [`RenderStore`] over the committed
/// folder, and the id of the last `msg` record written (the parent the next
/// append chains onto).
fn write_folder_atomic<M, U, T>(
    dir: &Path,
    session: &Session<M, U, T>,
) -> Result<(PathBuf, File, RenderStore, Option<MessageId>), SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    fs::create_dir_all(dir).map_err(StorageError::from)?;
    let folder = session_dir(dir, session.id);
    let tmp = tmp_sibling(&folder);
    let _ = fs::remove_dir_all(&tmp).ok();
    fs::create_dir_all(&tmp).map_err(StorageError::from)?;

    // meta.json: identity + summary. Lands first so subsequent event writes
    // never describe a session whose header is missing.
    atomic_write(&tmp.join(META_FILE_NAME), &serialize_header(session)?)?;

    let mut file = File::create(tmp.join(LOG_FILE_NAME)).map_err(StorageError::from)?;
    let mut renders = RenderStore::create(&tmp)?;

    let mut buf = Vec::new();
    let mut last_msg_id = None;
    for msg in session.messages.iter() {
        let id = MessageId::generate();
        append_record(
            &mut buf,
            &LogRecord::<&M>::Msg {
                id,
                parent_id: last_msg_id,
                timestamp: now_epoch(),
                d: msg,
            },
        )?;
        last_msg_id = Some(id);
    }
    for (id, output) in &session.tool_outputs {
        append_record(&mut buf, &LogRecord::<&M>::Out { id: id.clone() })?;
        renders.append(id, output.as_ref())?;
    }
    for (sub_id, msgs) in &session.subagent_messages {
        for msg in msgs.iter() {
            append_record(
                &mut buf,
                &LogRecord::<&M>::SubMsg {
                    sub: sub_id.clone(),
                    d: msg,
                },
            )?;
        }
    }
    file.write_all(&buf).map_err(StorageError::from)?;
    file.sync_data().map_err(StorageError::from)?;
    drop(file);
    drop(renders);

    verify_folder(&tmp, session)?;

    fsync_dir(&tmp)?;
    persist_folder(&tmp, &folder)?;
    fsync_dir(dir)?;

    let file = OpenOptions::new()
        .append(true)
        .open(folder.join(LOG_FILE_NAME))
        .map_err(StorageError::from)?;
    let renders = RenderStore::open(&folder)?;
    Ok((folder, file, renders, last_msg_id))
}

fn session_dir(dir: &Path, id: MakiId) -> PathBuf {
    dir.join(id.to_string())
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum V3RecordTag {
    Msg,
    Out,
    SubMsg {
        sub: String,
    },
    #[serde(other)]
    Other,
}

/// Verify the freshly written tmp folder against the in-memory session
/// before it replaces the live folder: record counts must match and every
/// `out` marker must have a frame in the renders index. Runs while both
/// representations exist, so a writer bug or a silently dropped render
/// surfaces here instead of as a fail-soft-loaded session.
fn verify_folder<M, U, T>(tmp: &Path, session: &Session<M, U, T>) -> Result<(), SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    #[derive(Deserialize)]
    struct MetaIdProbe {
        id: MakiId,
    }
    let meta: MetaIdProbe =
        serde_json::from_slice(&fs::read(tmp.join(META_FILE_NAME)).map_err(StorageError::from)?)
            .map_err(StorageError::from)?;
    if meta.id != session.id {
        return Err(SessionError::Verify(format!(
            "meta.json claims id {}, but the session is {}",
            meta.id, session.id
        )));
    }
    let log_bytes = fs::read(tmp.join(LOG_FILE_NAME)).map_err(StorageError::from)?;
    let mut msg_count = 0usize;
    let mut out_count = 0usize;
    let mut sub_msg_counts: HashMap<String, usize> = HashMap::new();
    for line in log_bytes.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<V3RecordTag>(line) {
            Ok(V3RecordTag::Msg) => msg_count += 1,
            Ok(V3RecordTag::Out) => out_count += 1,
            Ok(V3RecordTag::SubMsg { sub }) => *sub_msg_counts.entry(sub).or_insert(0usize) += 1,
            Ok(V3RecordTag::Other) => {}
            Err(e) => {
                return Err(SessionError::Verify(format!(
                    "unparseable record in {}: {e}",
                    tmp.join(LOG_FILE_NAME).display()
                )));
            }
        }
    }
    if msg_count != session.messages.len() {
        return Err(SessionError::Verify(format!(
            "expected {} msg records, found {msg_count}",
            session.messages.len()
        )));
    }
    if out_count != session.tool_outputs.len() {
        return Err(SessionError::Verify(format!(
            "expected {} out records, found {out_count}",
            session.tool_outputs.len()
        )));
    }
    if sub_msg_counts.len() != session.subagent_messages.len()
        || session
            .subagent_messages
            .iter()
            .any(|(sub, msgs)| sub_msg_counts.get(sub) != Some(&msgs.len()))
    {
        return Err(SessionError::Verify(
            "sub_msg record counts differ from the in-memory session".into(),
        ));
    }
    let renders = RenderStore::open(tmp)?;
    for id in session.tool_outputs.keys() {
        if !renders.contains(id) {
            return Err(SessionError::Verify(format!(
                "render frame for {id} missing from the renders index"
            )));
        }
    }
    Ok(())
}

/// Read a session file, refusing sizes that no legitimate session can reach.
/// The whole file is parsed in memory, so the cap is the decompression-bomb
/// defense for `log.jsonl` / `meta.json` / legacy files.
pub(crate) fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, SessionError> {
    let len = fs::metadata(path).map_err(StorageError::from)?.len();
    if len > cap {
        return Err(StorageError::FileTooLarge {
            path: path.display().to_string(),
            len,
            cap,
        }
        .into());
    }
    Ok(fs::read(path).map_err(StorageError::from)?)
}

enum MsgProbe {
    Found(MessageId),
    MsgWithoutId,
    NotMsg,
}

fn probe_msg_line(line: &[u8]) -> MsgProbe {
    #[derive(Deserialize)]
    struct MsgIdOnly {
        id: Option<MessageId>,
    }
    // The tag check reuses `V3RecordTag` so the record-family shape lives in
    // one place; the id is parsed from the same line on Msg records only.
    match serde_json::from_slice::<V3RecordTag>(line) {
        Ok(V3RecordTag::Msg) => match serde_json::from_slice::<MsgIdOnly>(line) {
            Ok(p) => match p.id {
                Some(id) => MsgProbe::Found(id),
                None => MsgProbe::MsgWithoutId,
            },
            Err(_) => MsgProbe::NotMsg,
        },
        _ => MsgProbe::NotMsg,
    }
}

/// Id of the last `msg` record in `log.jsonl`: the parent the next append
/// must chain onto. Walks the file backwards in chunks so a single oversized
/// trailing record (e.g. a huge `sub_msg`) cannot hide the last message.
fn last_msg_id(folder: &Path) -> Option<MessageId> {
    const PROBE_CHUNK: u64 = 64 * 1024;

    let mut file = File::open(folder.join(LOG_FILE_NAME)).ok()?;
    let mut end = file.metadata().ok()?.len();
    let mut pending = Vec::new();
    while end > 0 {
        let start = end.saturating_sub(PROBE_CHUNK);
        let mut buf = vec![0u8; (end - start) as usize];
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_exact(&mut buf).ok()?;
        end = start;

        let mut pos = buf.len();
        while let Some(nl) = buf[..pos].iter().rposition(|&b| b == b'\n') {
            let mut line = Vec::with_capacity(pos - nl - 1 + pending.len());
            line.extend_from_slice(&buf[nl + 1..pos]);
            line.extend_from_slice(&pending);
            pending.clear();
            match probe_msg_line(&line) {
                MsgProbe::Found(id) => return Some(id),
                MsgProbe::MsgWithoutId => return None,
                MsgProbe::NotMsg => {}
            }
            pos = nl;
        }
        pending.splice(0..0, buf[..pos].iter().copied());
    }
    match probe_msg_line(&pending) {
        MsgProbe::Found(id) => Some(id),
        _ => None,
    }
}

fn lock_path(dir: &Path, id: MakiId) -> PathBuf {
    dir.join(format!("{id}.lock"))
}

pub(crate) fn lock_session(dir: &Path, id: MakiId) -> Result<File, SessionError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path(dir, id))
        .map_err(StorageError::from)?;
    file.try_lock().map_err(|_| SessionError::Locked { id })?;
    Ok(file)
}

/// Recovered event slice from `log.jsonl`: messages in order, the deduped list
/// of `out` ids (payloads live in `renders.zst`), and sub-agent message maps.
type ParsedEvents<M> = (Vec<M>, Vec<String>, HashMap<String, Vec<M>>);

/// Parse the events-only `log.jsonl` byte slice (torn tail already truncated).
/// `Msg` / `Out` / `SubMsg` records are the only legal record types here;
/// anything else is a forward-compat or corruption artifact, logged + skipped.
fn parse_events<M>(data: &[u8], display_path: &str) -> Result<ParsedEvents<M>, SessionError>
where
    M: DeserializeOwned,
{
    let mut messages = Vec::new();
    let mut out_ids = Vec::new();
    let mut subagent_messages: HashMap<String, Vec<M>> = HashMap::new();

    for (i, line) in data.split(|&b| b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let record: LogRecord<M> = match serde_json::from_slice(line) {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    path = display_path,
                    error = %e,
                    line = i + 1,
                    "skipping unrecognized JSONL record",
                );
                continue;
            }
        };
        match record {
            LogRecord::Msg { d, .. } => messages.push(d),
            LogRecord::Out { id: out_id } => {
                if !out_ids.contains(&out_id) {
                    out_ids.push(out_id);
                }
            }
            LogRecord::SubMsg { sub, d } => {
                subagent_messages.entry(sub).or_default().push(d);
            }
        }
    }

    Ok((messages, out_ids, subagent_messages))
}

/// Load the canonical `<id>/` folder: read `meta.json` for the
/// [`SessionHeader`], parse `log.jsonl` (in memory, with the torn tail
/// truncated) to recover events + `out` ids, then decode each `out` frame
/// from `renders.zst` to fill `tool_outputs`.
fn load_folder<M, U, T>(folder: &Path, heal: bool) -> Result<Session<M, U, T>, SessionError>
where
    M: DeserializeOwned + Serialize + Clone,
    U: DeserializeOwned + Default + Serialize,
    T: DeserializeOwned + Serialize,
{
    let header: SessionHeader<U> = {
        let bytes = read_capped(&folder.join(META_FILE_NAME), MAX_META_BYTES)?;
        serde_json::from_slice(&bytes).map_err(StorageError::from)?
    };
    if header.log_format_version != LOG_FORMAT_VERSION {
        return Err(SessionError::VersionMismatch {
            found: header.log_format_version,
            expected: LOG_FORMAT_VERSION,
        });
    }
    // The folder name is the session's id; a meta.json claiming a different
    // id (or a non-id name) would alias locks and let content land under
    // another session's lock.
    let folder_name = folder
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if folder_name.parse::<MakiId>().ok() != Some(header.id) {
        return Err(SessionError::HeaderIdMismatch {
            folder: folder.display().to_string(),
            claimed: header.id,
            folder_name: folder_name.to_string(),
        });
    }

    let path = folder.join(LOG_FILE_NAME);
    let display_path = path.display().to_string();
    let bytes = read_capped(&path, MAX_LOG_BYTES)?;
    let valid = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);

    if heal && valid < bytes.len() {
        warn!(
            path = %display_path,
            tail_bytes = bytes.len() - valid,
            "truncating torn session log tail",
        );
        OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(StorageError::from)?
            .set_len(valid as u64)
            .map_err(StorageError::from)?;
    }

    // Without the lock a torn tail is a live writer's in-flight record; leave
    // it on disk and let `parse_events` skip the partial line.
    let end = if heal { valid } else { bytes.len() };
    let (messages, out_ids, subagent_messages) = parse_events::<M>(&bytes[..end], &display_path)?;

    let mut tool_outputs = HashMap::with_capacity(out_ids.len());
    let mut total_output_bytes = 0usize;
    let renders = (if heal {
        RenderStore::open(folder).map(Some)
    } else {
        RenderStore::open_readonly(folder)
    })
    .unwrap_or_else(|e| {
        warn!(
            error = %e,
            path = %display_path,
            "render store unreadable; dropping all tool outputs",
        );
        None
    });
    if let Some(mut renders) = renders {
        for id in &out_ids {
            match renders.get::<T>(id) {
                Ok(Some(value)) => {
                    let size = serde_json::to_vec(&value)
                        .map_err(StorageError::from)?
                        .len();
                    if total_output_bytes + size > MAX_TOTAL_TOOL_OUTPUT_BYTES {
                        warn!(
                            path = %display_path,
                            "aggregate tool output size cap exceeded; dropping remaining frames",
                        );
                        break;
                    }
                    total_output_bytes += size;
                    tool_outputs.insert(id.clone(), Arc::new(value));
                }
                Ok(None) => warn!(
                    path = %display_path,
                    tool_use_id = %id,
                    "out marker has no matching render frame; dropping",
                ),
                Err(e) => warn!(
                    error = %e,
                    tool_use_id = %id,
                    "render frame failed to decode; dropping",
                ),
            }
        }
    }

    Ok(Session::from_parts(
        header,
        messages,
        tool_outputs,
        subagent_messages,
    ))
}

fn load_cwd_index(dir: &Path) -> HashMap<String, String> {
    fs::read(dir.join(CWD_INDEX_FILE))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn update_cwd_index(dir: &Path, cwd: &str, session_id: MakiId) -> Result<(), StorageError> {
    let mut index = load_cwd_index(dir);
    let id_str = session_id.to_string();
    if index.get(cwd).is_some_and(|v| *v == id_str) {
        return Ok(());
    }
    index.insert(cwd.to_string(), id_str);
    atomic_write(&dir.join(CWD_INDEX_FILE), &serde_json::to_vec(&index)?)
}

fn remove_from_cwd_index(dir: &Path, session_id: MakiId) -> Result<(), StorageError> {
    let mut index = load_cwd_index(dir);
    let before = index.len();
    index.retain(|_, v| v.parse::<MakiId>() != Ok(session_id));
    if index.len() != before {
        atomic_write(&dir.join(CWD_INDEX_FILE), &serde_json::to_vec(&index)?)?;
    }
    Ok(())
}

// -- Header scanning for session list --

/// Cached scan result for one session entry, keyed by file name and validated
/// by (size, mtime): stale entries are rescanned, deleted files pruned.
/// `header: None` marks files that failed to scan (wrong version, foreign
/// format), so they are not re-read on every list either.
#[derive(Serialize, Deserialize)]
struct ScanCacheEntry {
    size: u64,
    mtime_ms: u64,
    header: Option<ScannedHeader>,
}

/// Identity + summary extracted by scanning one session entry; cached in
/// `scan_cache.json` so unchanged entries are not re-read on every list.
#[derive(Serialize, Deserialize)]
pub(crate) struct ScannedHeader {
    pub(crate) id: MakiId,
    pub(crate) cwd: String,
    pub(crate) title: String,
    pub(crate) updated_at: u64,
}

type ScanCache = HashMap<String, ScanCacheEntry>;

fn load_scan_cache(dir: &Path) -> ScanCache {
    fs::read(dir.join(SCAN_CACHE_FILE))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn file_signature(path: &Path) -> Option<(u64, u64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)?;
    Some((meta.len(), mtime_ms))
}

fn scan_headers(cwd: &str, dir: &Path) -> Result<Vec<SessionSummary>, StorageError> {
    sweep_stray_dirs(dir);
    let mut cache = load_scan_cache(dir);
    let mut fresh = ScanCache::new();
    let mut dirty = false;
    let mut out = Vec::new();
    for path in session_entries(dir)? {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let signature_path = if path.is_dir() {
            // v3 folders mutate `meta.json` and `log.jsonl` independently; the
            // signature keys on `meta.json`, which drives the picker (title,
            // updated_at). Legacy flat files sign themselves.
            Cow::Owned(path.join(META_FILE_NAME))
        } else {
            Cow::Borrowed(&path)
        };
        let Some((size, mtime_ms)) = file_signature(&signature_path) else {
            continue;
        };
        let entry = match cache.remove(name) {
            Some(e) if e.size == size && e.mtime_ms == mtime_ms => e,
            _ => {
                dirty = true;
                let header = scan_entry_header(&path);
                ScanCacheEntry {
                    size,
                    mtime_ms,
                    header,
                }
            }
        };
        if let Some(h) = &entry.header
            && h.cwd == cwd
        {
            out.push(SessionSummary {
                id: h.id,
                title: normalize_title(&h.title),
                updated_at: h.updated_at,
            });
        }
        fresh.insert(name.to_owned(), entry);
    }
    // Leftover cache entries belong to deleted files; rewriting prunes them.
    if (dirty || !cache.is_empty())
        && let Ok(data) = serde_json::to_vec(&fresh)
        && let Err(e) = atomic_write(&dir.join(SCAN_CACHE_FILE), &data)
    {
        warn!(error = %e, "failed to write session scan cache");
    }
    Ok(out)
}

pub(crate) fn session_entries(dir: &Path) -> Result<Vec<PathBuf>, StorageError> {
    Ok(fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|p| is_session_entry(p))
        .collect())
}

/// A child of `sessions/` worth scanning: a session directory (one that
/// contains `log.jsonl`), or a legacy flat `.json` / `.jsonl` file. Excludes
/// the cwd index and scan cache whatever their extension, and skips any dir
/// whose name is a migration tmp or swap suffix (e.g. `<id>.tmp`, `<id>.old`)
/// so an interrupted folder swap does not double-list the session.
fn is_session_entry(p: &Path) -> bool {
    let stem = p.file_stem().and_then(|s| s.to_str());
    let is_index = stem.is_some_and(|s| NON_SESSION_STEMS.contains(&s));
    if is_index {
        return false;
    }
    if p.is_dir() {
        if p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(TMP_DIR_SUFFIX) || n.ends_with(OLD_DIR_SUFFIX))
        {
            return false;
        }
        return p.join(LOG_FILE_NAME).exists();
    }
    p.extension().is_some_and(|e| e == "json" || e == "jsonl")
}

// -- Session impl --

impl<M, U, T> Session<M, U, T> {
    pub fn messages(&self) -> &[M] {
        &self.messages
    }

    pub fn take_messages(self) -> Vec<M>
    where
        M: Clone,
    {
        Arc::unwrap_or_clone(self.messages)
    }

    pub fn tool_outputs(&self) -> &HashMap<String, Arc<T>> {
        &self.tool_outputs
    }

    pub fn subagent_messages(&self) -> &HashMap<String, Arc<Vec<M>>> {
        &self.subagent_messages
    }
}

impl<M, U, T> Session<M, U, T> {
    /// Assemble a freshly loaded session from its header and parsed event
    /// state; used by the loaders in [`crate::migration`]. Fields stay
    /// private so runtime mutation still goes through the classifying
    /// mutators.
    pub(crate) fn from_parts(
        header: SessionHeader<U>,
        messages: Vec<M>,
        tool_outputs: HashMap<String, Arc<T>>,
        subagent_messages: HashMap<String, Vec<M>>,
    ) -> Self {
        Self {
            version: SESSION_VERSION,
            id: header.id,
            title: header.title,
            cwd: header.cwd,
            model: header.model,
            messages: Arc::new(messages),
            token_usage: header.token_usage,
            tool_outputs,
            subagent_messages: subagent_messages
                .into_iter()
                .map(|(id, msgs)| (id, Arc::new(msgs)))
                .collect(),
            subagents: header.subagents,
            usage_by_model: header.usage_by_model,
            meta: header.meta,
            created_at: header.created_at,
            updated_at: header.updated_at,
            revision: 0,
            content_revision: 0,
            epoch: next_epoch(),
        }
    }
}

impl<M, U, T> Session<M, U, T>
where
    M: Serialize + DeserializeOwned + TitleSource + Clone,
    U: Serialize + DeserializeOwned + Default,
    T: Serialize + DeserializeOwned,
{
    pub fn new(model: &str, cwd: &str) -> Self {
        let now = now_epoch();
        Self {
            version: SESSION_VERSION,
            id: MakiId::generate(),
            title: DEFAULT_TITLE.into(),
            cwd: cwd.into(),
            model: model.into(),
            messages: Arc::default(),
            token_usage: U::default(),
            tool_outputs: HashMap::new(),
            subagent_messages: HashMap::new(),
            subagents: Vec::new(),
            usage_by_model: HashMap::new(),
            meta: SessionMeta {
                mode: Some(StoredMode::Build),
                ..Default::default()
            },
            created_at: now,
            updated_at: now,
            revision: 0,
            content_revision: 0,
            epoch: next_epoch(),
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn content_revision(&self) -> u64 {
        self.content_revision
    }

    fn touch(&mut self) {
        self.content_revision += 1;
        self.touch_soft();
    }

    /// Only UI state moved, so the write can wait for company. Everything a
    /// crash would lose for good goes through `touch`, which is the default a
    /// new mutator gets by not thinking about it.
    fn touch_soft(&mut self) {
        self.updated_at = now_epoch();
        self.revision += 1;
    }

    /// Every append cursor into the log is void from here on.
    fn rewrite(&mut self) {
        self.epoch = next_epoch();
        self.touch();
    }

    pub fn push_message(&mut self, msg: M) {
        Arc::make_mut(&mut self.messages).push(msg);
        self.touch();
    }

    pub fn replace_messages(&mut self, messages: Vec<M>) {
        self.messages = Arc::new(messages);
        self.rewrite();
    }

    pub fn truncate_messages(&mut self, len: usize) {
        if len >= self.messages.len() {
            return;
        }
        Arc::make_mut(&mut self.messages).truncate(len);
        self.rewrite();
    }

    /// Adopting a producer's snapshot inherits its run token, so the log's
    /// cursors survive exactly when the snapshot was an append.
    fn set_history(&mut self, snapshot: &HistorySnapshot<M>) {
        self.messages = Arc::clone(&snapshot.messages);
        self.epoch = snapshot.epoch;
        self.touch();
    }

    /// Applies everything the owner mirrors from live state. It takes an `Arc`
    /// and checks for a real change first because `Arc::make_mut` deep-copies
    /// the whole session while the writer still holds the last snapshot, and an
    /// idle session should not pay for that every frame.
    pub fn checkpoint(
        this: &mut Arc<Self>,
        history: Option<&HistorySnapshot<M>>,
        meta: SessionMeta,
        token_usage: U,
    ) where
        M: Clone,
        U: PartialEq + Clone,
        T: Clone,
    {
        let history = history.filter(|h| !Arc::ptr_eq(&this.messages, &h.messages));
        if history.is_none() && this.meta == meta && this.token_usage == token_usage {
            return;
        }
        let session = Arc::make_mut(this);
        if let Some(snapshot) = history {
            session.set_history(snapshot);
            // The title comes from the messages, so it goes stale exactly when
            // they move.
            session.update_title_if_default();
        }
        session.set_meta(meta);
        session.set_token_usage(token_usage);
    }

    /// A change under an existing id is not expressible as an append, so it
    /// voids the cursors; a new id is a pure append.
    pub fn insert_tool_output(&mut self, id: String, output: T) {
        if self.tool_outputs.insert(id, Arc::new(output)).is_some() {
            self.rewrite();
        } else {
            self.touch();
        }
    }

    pub fn set_subagent_messages(&mut self, id: String, msgs: Vec<M>) {
        if self.subagent_messages.insert(id, Arc::new(msgs)).is_some() {
            self.rewrite();
        } else {
            self.touch();
        }
    }

    fn set_token_usage(&mut self, usage: U)
    where
        U: PartialEq,
    {
        if self.token_usage == usage {
            return;
        }
        self.token_usage = usage;
        self.touch();
    }

    fn set_meta(&mut self, meta: SessionMeta) {
        if self.meta == meta {
            return;
        }
        self.meta = meta;
        self.touch_soft();
    }

    pub fn subagents(&self) -> &[StoredSubagent] {
        &self.subagents
    }

    pub fn set_subagents(&mut self, subagents: Vec<StoredSubagent>) {
        if self.subagents == subagents {
            return;
        }
        self.subagents = subagents;
        self.touch();
    }

    pub fn usage_by_model(&self) -> &HashMap<String, StoredTokenUsage> {
        &self.usage_by_model
    }

    pub fn set_title(&mut self, title: String) {
        if self.title == title {
            return;
        }
        self.title = title;
        self.touch();
    }

    /// Header field: appends never rewrite the header, so the change voids
    /// the cursors to force a full rewrite.
    pub fn set_cwd(&mut self, cwd: String) {
        if self.cwd == cwd {
            return;
        }
        self.cwd = cwd;
        self.rewrite();
    }

    /// Header field, see [`Self::set_cwd`].
    pub fn set_model(&mut self, model: String) {
        if self.model == model {
            return;
        }
        self.model = model;
        self.rewrite();
    }

    pub fn add_model_usage(&mut self, model: &str, usage: StoredTokenUsage) {
        *self.usage_by_model.entry(model.to_owned()).or_default() += usage;
        self.touch();
    }

    /// After `messages` is truncated (rewind), state keyed by tool_use_id can
    /// point at calls that no longer exist. On restore that shows up as ghost
    /// subagent tabs and leaked tool outputs, so this drops everything not
    /// reachable from `messages`.
    ///
    /// If you add another field keyed by tool_use_id, prune it here too.
    pub fn prune_orphans(&mut self, tool_ids: impl Fn(&M) -> Vec<String>) {
        let main_ids: HashSet<String> = self.messages.iter().flat_map(&tool_ids).collect();
        self.subagent_messages.retain(|id, _| main_ids.contains(id));
        self.subagents
            .retain(|sa| main_ids.contains(&sa.tool_use_id));

        let live: HashSet<String> = self
            .subagent_messages
            .values()
            .flat_map(|msgs| msgs.iter())
            .flat_map(&tool_ids)
            .chain(main_ids)
            .collect();
        self.tool_outputs.retain(|id, _| live.contains(id));
        self.rewrite();
    }

    pub fn load(id: MakiId, dir: &StateDir) -> Result<Self, SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        Self::load_from(id, &sessions_dir)
    }

    pub fn load_from(id: MakiId, dir: &Path) -> Result<Self, SessionError> {
        let folder = session_dir(dir, id);
        // Healing (`.old` recovery, torn-tail truncation, renders heal) must
        // not race a live writer, so it only runs under the session lock.
        // When the lock is held elsewhere the folder is read read-only and
        // left untouched; a swap in progress surfaces as Locked instead of a
        // phantom NotFound.
        let lock = lock_session(dir, id).ok();
        let old = old_sibling(&folder);
        if lock.is_none() && !folder.is_dir() && old.is_dir() {
            return Err(SessionError::Locked { id });
        }
        if lock.is_some()
            && !folder.is_dir()
            && old.is_dir()
            && let Err(e) = fs::rename(&old, &folder)
        {
            warn!(error = %e, "failed to recover crashed folder swap");
        }
        if folder.is_dir() {
            // v3 folders carry `meta.json`. A folder without one is not a
            // session (only intermediate branch builds produced those) and is
            // left untouched, invisible to the picker and unloadable.
            if !folder.join(META_FILE_NAME).exists() {
                return Err(StorageError::NotFound(id.to_string()).into());
            }
            let mut session = load_folder::<M, U, T>(&folder, lock.is_some())?;
            session.title = normalize_title(&session.title);
            if lock.is_some()
                && let Err(e) = remove_legacy_files(dir, id)
            {
                warn!(error = %e, "legacy files remain after folder load");
            }
            return Ok(session);
        }

        // No folder: load the legacy flat file (`.json` or `.jsonl`, possibly
        // under a hex-id name) fully, then write the canonical folder from it.
        // The early lock covered the requested id, which may differ from the
        // legacy header's id; drop it and let the migration lock for itself.
        drop(lock);
        let Some(legacy_path) = legacy_flat_file(dir, id) else {
            return Err(StorageError::NotFound(id.to_string()).into());
        };
        let session = load_session_at::<M, U, T>(&legacy_path)?;
        match lock_session(dir, session.id) {
            Ok(lock) => {
                if let Err(e) = SessionLog::write_canonical(dir, &session, lock) {
                    warn!(error = %e, "failed to migrate legacy session; keeping legacy file");
                }
            }
            Err(e) => warn!(error = %e, "session locked; skipping legacy migration"),
        }
        Ok(session)
    }

    pub fn list(cwd: &str, dir: &StateDir) -> Result<Vec<SessionSummary>, SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        Self::list_in(cwd, &sessions_dir)
    }

    pub fn list_in(cwd: &str, dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
        let mut summaries = scan_headers(cwd, dir)?;
        summaries.sort_unstable_by_key(|s| Reverse(s.updated_at));
        Ok(summaries)
    }

    pub fn latest(cwd: &str, dir: &StateDir) -> Result<Option<Self>, SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        Self::latest_in(cwd, &sessions_dir)
    }

    pub fn latest_in(cwd: &str, dir: &Path) -> Result<Option<Self>, SessionError> {
        let cached = load_cwd_index(dir)
            .remove(cwd)
            .and_then(|s| match s.parse::<MakiId>() {
                Ok(id) => Some(id),
                Err(e) => {
                    warn!(error = %e, cwd, "indexed session id unparseable; rescanning");
                    None
                }
            });
        if let Some(id) = cached {
            match Self::load_from(id, dir) {
                Ok(s) => return Ok(Some(s)),
                Err(e) => warn!(error = %e, cwd, "indexed session missing on disk; rescanning"),
            }
        }

        scan_headers(cwd, dir)?
            .into_iter()
            .max_by_key(|s| s.updated_at)
            .map(|s| Self::load_from(s.id, dir).map(Some))
            .unwrap_or(Ok(None))
    }

    pub fn update_title_if_default(&mut self) {
        if self.title == DEFAULT_TITLE {
            self.set_title(generate_title(&self.messages));
        }
    }

    pub fn delete(id: MakiId, dir: &StateDir) -> Result<(), SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        Self::delete_from(id, &sessions_dir)
    }

    pub fn delete_from(id: MakiId, dir: &Path) -> Result<(), SessionError> {
        let _lock = lock_session(dir, id)?;
        let folder = session_dir(dir, id);
        let old = old_sibling(&folder);
        let tmp = tmp_sibling(&folder);
        let mut removed = try_remove_dir_all(&folder)?;
        removed |= remove_legacy_files(dir, id)?;
        // A crashed swap can leave the session's state in the siblings;
        // delete those too or the session resurrects on the next load.
        removed |= old.is_dir() || tmp.is_dir();
        let _ = fs::remove_dir_all(&old);
        let _ = fs::remove_dir_all(&tmp);
        let _ = fs::remove_file(lock_path(dir, id));
        if !removed {
            return Err(StorageError::NotFound(id.to_string()).into());
        }
        remove_from_cwd_index(dir, id)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    use crate::renders::{RENDERS_FILE_NAME, RENDERS_MAGIC, RenderError};
    use serde_json::Value;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct TestMsg {
        text: String,
    }

    impl TitleSource for TestMsg {
        fn first_user_text(&self) -> Option<&str> {
            Some(&self.text)
        }
    }

    type TestSession = Session<TestMsg, Value, Value>;

    fn user_message(text: &str) -> TestMsg {
        TestMsg { text: text.into() }
    }

    fn new_session() -> TestSession {
        let mut session = TestSession::new("test-model", "/project");
        session.messages = Arc::new(vec![user_message("hello")]);
        session
    }

    fn assert_same_content(loaded: &TestSession, expected: &TestSession) {
        assert_eq!(loaded.id, expected.id);
        assert_eq!(loaded.title, expected.title);
        assert_eq!(loaded.messages(), expected.messages());
        assert_eq!(loaded.tool_outputs().len(), expected.tool_outputs().len());
        assert_eq!(
            loaded.subagent_messages().len(),
            expected.subagent_messages().len()
        );
        assert_eq!(loaded.meta, expected.meta);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session = new_session();
        session.set_title("t".into());
        session.insert_tool_output("toolu_1".into(), serde_json::json!({"v":1}));
        session.set_subagent_messages("sub-1".into(), vec![user_message("sub")]);
        SessionLog::rewrite(dir, &session).unwrap();

        let loaded = TestSession::load_from(session.id, dir).unwrap();

        assert_same_content(&loaded, &session);
        assert_eq!(loaded.tool_outputs()["toolu_1"]["v"], 1);
        assert_eq!(loaded.subagent_messages()["sub-1"].len(), 1);
    }

    #[test]
    fn append_writes_delta_and_load_recovers() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session = new_session();
        let log = SessionLog::rewrite(dir, &session).unwrap();
        let mut log = log;

        session.push_message(user_message("second"));
        session.insert_tool_output("toolu_2".into(), serde_json::json!({"v":2}));
        log.append(&session).unwrap();
        drop(log);

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_same_content(&loaded, &session);
        assert_eq!(loaded.tool_outputs()["toolu_2"]["v"], 2);
    }

    #[test]
    fn load_from_does_not_heal_while_locked() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        SessionLog::rewrite(dir, &session).unwrap();

        let folder = dir.join(session.id.to_string());
        let log_path = folder.join(LOG_FILE_NAME);
        OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap()
            .write_all(b"{\"t\":\"msg\",\"id\":\"msg_\"")
            .unwrap();

        let lock = lock_session(dir, session.id).unwrap();
        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_same_content(&loaded, &session);
        let bytes = fs::read(&log_path).unwrap();
        assert_ne!(
            bytes.last(),
            Some(&b'\n'),
            "torn tail must not be truncated while locked"
        );
        drop(lock);

        TestSession::load_from(session.id, dir).unwrap();
        let bytes = fs::read(&log_path).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'), "torn tail healed once unlocked");
    }

    #[test]
    fn load_from_while_locked_without_folder_is_locked_error() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        SessionLog::rewrite(dir, &session).unwrap();

        let folder = dir.join(session.id.to_string());
        let old = old_sibling(&folder);
        let lock = lock_session(dir, session.id).unwrap();
        fs::rename(&folder, &old).unwrap();

        let err = TestSession::load_from(session.id, dir).unwrap_err();
        assert!(matches!(err, SessionError::Locked { id } if id == session.id));

        fs::rename(&old, &folder).unwrap();
        drop(lock);
    }

    #[test]
    fn verify_folder_rejects_missing_render_frame() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session = new_session();
        session.insert_tool_output("toolu_1".into(), serde_json::json!({"v": 1}));
        SessionLog::rewrite(dir, &session).unwrap();

        let folder = dir.join(session.id.to_string());
        let mut header = vec![b'm', b'k', b'f', b'r', LOG_FORMAT_VERSION as u8];
        fs::write(folder.join(RENDERS_FILE_NAME), &mut header).unwrap();

        let err = verify_folder(&folder, &session).unwrap_err();
        assert!(matches!(err, SessionError::Verify(_)));
    }

    #[test]
    fn verify_folder_rejects_missing_out_marker() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session = new_session();
        session.insert_tool_output("toolu_1".into(), serde_json::json!({"v": 1}));
        SessionLog::rewrite(dir, &session).unwrap();

        let folder = dir.join(session.id.to_string());
        let log_path = folder.join(LOG_FILE_NAME);
        let kept: String = fs::read_to_string(&log_path)
            .unwrap()
            .lines()
            .filter(|line| !line.contains("\"t\":\"out\""))
            .map(|line| format!("{line}\n"))
            .collect();
        fs::write(&log_path, kept).unwrap();

        let err = verify_folder(&folder, &session).unwrap_err();
        assert!(matches!(err, SessionError::Verify(_)));
    }

    #[test]
    fn verify_folder_rejects_meta_id_mismatch() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        SessionLog::rewrite(dir, &session).unwrap();

        let folder = dir.join(session.id.to_string());
        let meta_path = folder.join(META_FILE_NAME);
        let mut meta: serde_json::Value =
            serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        meta["id"] = MakiId::generate().to_string().into();
        fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();

        let err = verify_folder(&folder, &session).unwrap_err();
        assert!(matches!(err, SessionError::Verify(_)));
    }

    #[test]
    fn load_rejects_meta_claiming_another_sessions_id() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        SessionLog::rewrite(dir, &session).unwrap();

        let folder = dir.join(session.id.to_string());
        let meta_path = folder.join(META_FILE_NAME);
        let mut meta: serde_json::Value =
            serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        meta["id"] = MakiId::generate().to_string().into();
        fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();

        let err = TestSession::load_from(session.id, dir).unwrap_err();
        assert!(matches!(err, SessionError::HeaderIdMismatch { .. }));
    }

    #[test]
    fn delete_removes_crashed_swap_siblings() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        SessionLog::rewrite(dir, &session).unwrap();

        let folder = dir.join(session.id.to_string());
        let old = old_sibling(&folder);
        fs::rename(&folder, &old).unwrap();

        TestSession::delete_from(session.id, dir).unwrap();
        assert!(
            !old.exists(),
            "swap sibling must be deleted with the session"
        );
        assert!(matches!(
            TestSession::load_from(session.id, dir),
            Err(SessionError::Storage(StorageError::NotFound(_)))
        ));
    }

    #[test]
    fn sweep_skips_locked_tmp_and_removes_stale() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        SessionLog::rewrite(dir, &session).unwrap();

        let folder = dir.join(session.id.to_string());
        let stale_tmp = tmp_sibling(&folder);
        fs::create_dir_all(&stale_tmp).unwrap();

        let lock = lock_session(dir, session.id).unwrap();
        sweep_stray_dirs(dir);
        assert!(stale_tmp.exists(), "locked session's tmp must not be swept");

        drop(lock);
        sweep_stray_dirs(dir);
        assert!(!stale_tmp.exists(), "stale tmp swept once unlocked");
    }

    #[test]
    fn renders_open_refuses_future_version() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session = new_session();
        session.insert_tool_output("toolu_1".into(), serde_json::json!({"v": 1}));
        SessionLog::rewrite(dir, &session).unwrap();

        let folder = dir.join(session.id.to_string());
        let renders_path = folder.join(RENDERS_FILE_NAME);
        let mut data = fs::read(&renders_path).unwrap();
        data[RENDERS_MAGIC.len()] = LOG_FORMAT_VERSION as u8 + 1;
        fs::write(&renders_path, &data).unwrap();

        let err = match RenderStore::open(&folder) {
            Ok(_) => panic!("expected version mismatch"),
            Err(e) => e,
        };
        assert!(matches!(err, RenderError::VersionMismatch { .. }));
        assert_eq!(
            fs::read(&renders_path).unwrap(),
            data,
            "wrong-version renders must be left untouched"
        );
    }

    #[test]
    fn migration_failure_keeps_legacy_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let id: MakiId = "01965087-4c71-7f00-8000-000000000000".parse().unwrap();
        let mut session = new_session();
        session.id = id;
        // A tool id longer than the renders id cap makes the folder write
        // fail after the legacy file was read, so migration must back off.
        session.insert_tool_output("t".repeat(300), serde_json::json!({"v": 1}));
        let legacy_path = dir.join(format!("{id}.json"));
        fs::write(&legacy_path, serde_json::to_vec(&session).unwrap()).unwrap();

        let loaded = TestSession::load_from(id, dir).unwrap();
        assert_eq!(loaded.messages().len(), 1);
        assert!(
            legacy_path.exists(),
            "legacy file kept when migration fails"
        );
    }

    #[test]
    fn meta_json_rewritten_on_title_change_only() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session = new_session();
        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        session.set_title("renamed".into());
        log.append(&session).unwrap();

        let meta_path = dir.join(session.id.to_string()).join(META_FILE_NAME);
        let header: SessionHeader<Value> =
            serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        assert_eq!(header.title, "renamed");

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(loaded.title, "renamed");
    }

    #[test]
    fn rewind_diverges_and_rewrite_recovers() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session = new_session();
        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        session.push_message(user_message("second"));
        session.push_message(user_message("third"));
        log.append(&session).unwrap();

        // Rewind voids the append cursors.
        session.truncate_messages(1);
        session.push_message(user_message("replaced"));
        assert!(matches!(
            log.append(&session),
            Err(SessionError::LogDiverged { .. })
        ));

        drop(log);
        let log = SessionLog::rewrite(dir, &session).unwrap();
        drop(log);
        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_same_content(&loaded, &session);
    }

    #[test]
    fn subagent_replacement_diverges() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session = new_session();
        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        session.set_subagent_messages("sub-1".into(), vec![user_message("old")]);
        log.append(&session).unwrap();

        session.set_subagent_messages("sub-1".into(), vec![user_message("new")]);
        assert!(matches!(
            log.append(&session),
            Err(SessionError::LogDiverged { .. })
        ));
    }

    #[test]
    fn torn_log_tail_truncated_on_load() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session = new_session();
        let mut log = SessionLog::rewrite(dir, &session).unwrap();
        session.push_message(user_message("second"));
        log.append(&session).unwrap();
        drop(log);

        let folder = dir.join(session.id.to_string());
        let log_path = folder.join(LOG_FILE_NAME);
        let mut bytes = fs::read(&log_path).unwrap();
        bytes.extend_from_slice(b"{\"t\":\"msg\",\"d\":{\"trun");
        fs::write(&log_path, &bytes).unwrap();

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(loaded.messages().len(), 2);
        assert!(!bytes_contain_partial(&fs::read(&log_path).unwrap()));
    }

    fn bytes_contain_partial(data: &[u8]) -> bool {
        data.iter()
            .rposition(|&b| b == b'\n')
            .is_some_and(|i| i + 1 < data.len())
    }

    #[test]
    fn delete_removes_folder_and_legacy_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        let id = session.id;
        SessionLog::rewrite(dir, &session).unwrap();

        // Stray legacy sibling to clean up.
        fs::write(dir.join(format!("{id}.jsonl")), "{}").unwrap();

        TestSession::delete_from(id, dir).unwrap();
        assert!(!dir.join(id.to_string()).exists());
        assert!(!dir.join(format!("{id}.jsonl")).exists());
        assert!(!dir.join(format!("{id}.lock")).exists());
    }

    #[test]
    fn load_migrates_legacy_jsonl_to_folder() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let id: MakiId = "01965087-4c71-7f00-8000-000000000000".parse().unwrap();
        let legacy = format!(
            "{{\"t\":\"header\",\"v\":2,\"id\":\"{id}\",\"model\":\"m\",\"cwd\":\"/project\",\"created_at\":0}}\n             {{\"t\":\"msg\",\"d\":{{\"text\":\"hello\"}}}}\n             {{\"t\":\"out\",\"id\":\"toolu_1\",\"d\":{{\"v\":1}}}}\n             {{\"t\":\"meta\",\"title\":\"t\",\"token_usage\":null,\"updated_at\":0}}\n"
        );
        fs::write(dir.join(format!("{id}.jsonl")), legacy).unwrap();

        let loaded = TestSession::load_from(id, dir).unwrap();

        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.tool_outputs()["toolu_1"]["v"], 1);
        assert_eq!(loaded.title, "t");
        assert!(dir.join(id.to_string()).is_dir());
        assert!(
            !dir.join(format!("{id}.jsonl")).exists(),
            "legacy file removed"
        );
    }

    #[test]
    fn persist_folder_swaps_existing_folder() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        let folder = dir.join(session.id.to_string());
        SessionLog::rewrite(dir, &session).unwrap();

        let mut session2 = session.clone();
        session2.push_message(user_message("more"));
        let tmp_dir = tmp_sibling(&folder);
        fs::create_dir_all(&tmp_dir).unwrap();
        fs::write(tmp_dir.join("marker"), "new").unwrap();

        persist_folder(&tmp_dir, &folder).unwrap();

        assert!(folder.join("marker").exists());
        assert!(!tmp_dir.exists());
        assert!(!dir.join(format!("{}.old", session.id)).exists());
    }

    #[test]
    fn scan_headers_lists_v3_folder_and_legacy() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        SessionLog::rewrite(dir, &session).unwrap();
        SessionLog::rewrite(dir, &session).unwrap();

        let summaries = TestSession::list_in("/project", dir).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, session.id);
    }

    #[test]
    fn crashed_folder_swap_recovers_on_load() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        SessionLog::rewrite(dir, &session).unwrap();

        let folder = dir.join(session.id.to_string());
        let old = old_sibling(&folder);
        fs::rename(&folder, &old).unwrap();

        let loaded = TestSession::load_from(session.id, dir).unwrap();

        assert_same_content(&loaded, &session);
        assert!(folder.is_dir());
    }

    #[test]
    fn scan_sweeps_stray_tmp_and_old_dirs() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        let id = session.id;
        SessionLog::rewrite(dir, &session).unwrap();

        let tmp_dir = dir.join(format!("{id}.tmp"));
        let old_dir = dir.join(format!("{id}.old"));
        let stray_old = dir.join(format!("{}.old", MakiId::generate()));
        fs::create_dir_all(&tmp_dir).unwrap();
        fs::create_dir_all(&old_dir).unwrap();
        fs::create_dir_all(&stray_old).unwrap();

        TestSession::list_in("/project", dir).unwrap();

        assert!(!tmp_dir.exists());
        assert!(!old_dir.exists());
        assert!(stray_old.exists());
    }

    #[test]
    fn second_cursor_is_refused_and_recovers() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        let log = SessionLog::rewrite(dir, &session).unwrap();

        let err = SessionLog::rewrite(dir, &session);
        assert!(matches!(err, Err(SessionError::Locked { id }) if id == session.id));

        drop(log);
        SessionLog::rewrite(dir, &session).unwrap();
    }

    #[test]
    fn delete_refused_while_locked() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        let log = SessionLog::rewrite(dir, &session).unwrap();

        let err = TestSession::delete_from(session.id, dir).unwrap_err();
        assert!(matches!(err, SessionError::Locked { id } if id == session.id));
        assert!(dir.join(session.id.to_string()).exists());

        drop(log);
        TestSession::delete_from(session.id, dir).unwrap();
        assert!(!dir.join(session.id.to_string()).exists());
    }

    #[test]
    fn load_drops_unreadable_render_but_keeps_session() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session = new_session();
        session.insert_tool_output("toolu_1".into(), serde_json::json!({"v":1}));
        SessionLog::rewrite(dir, &session).unwrap();

        let mut header = Vec::new();
        header.extend_from_slice(RENDERS_MAGIC.as_ref());
        header.push(LOG_FORMAT_VERSION as u8);
        fs::write(
            dir.join(session.id.to_string()).join(RENDERS_FILE_NAME),
            &header,
        )
        .unwrap();

        let loaded = TestSession::load_from(session.id, dir).unwrap();

        assert_eq!(loaded.messages(), session.messages());
        assert!(loaded.tool_outputs().is_empty());
    }

    #[test]
    fn load_skips_render_that_fails_to_decode() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session = new_session();
        session.insert_tool_output("toolu_x".into(), serde_json::json!({"v":1}));
        SessionLog::rewrite(dir, &session).unwrap();

        let id_bytes = b"toolu_x";
        let frame = structured_zstd::encoding::compress_slice_to_vec(
            b"not json",
            structured_zstd::encoding::CompressionLevel::Fastest,
        );
        let mut crc_input = Vec::new();
        crc_input.extend_from_slice(id_bytes);
        crc_input.extend_from_slice(&frame);
        let crc = crc32fast::hash(&crc_input);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(RENDERS_MAGIC.as_ref());
        bytes.push(LOG_FORMAT_VERSION as u8);
        bytes.push(id_bytes.len() as u8);
        bytes.extend_from_slice(id_bytes);
        bytes.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes.extend_from_slice(&frame);
        fs::write(
            dir.join(session.id.to_string()).join(RENDERS_FILE_NAME),
            &bytes,
        )
        .unwrap();

        let loaded = TestSession::load_from(session.id, dir).unwrap();

        assert_eq!(loaded.messages(), session.messages());
        assert!(loaded.tool_outputs().is_empty());
    }

    #[test]
    fn msg_records_form_a_linear_chain() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session = new_session();
        session.push_message(user_message("second"));
        let log = SessionLog::rewrite(dir, &session).unwrap();
        drop(log);

        let log_path = dir.join(session.id.to_string()).join(LOG_FILE_NAME);
        let records: Vec<LogRecord<TestMsg>> = fs::read_to_string(&log_path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        let LogRecord::Msg {
            id: first,
            parent_id,
            timestamp,
            ..
        } = &records[0]
        else {
            panic!("expected msg record");
        };
        assert_eq!(*parent_id, None);
        assert!(*timestamp > 0);
        let LogRecord::Msg {
            id: second,
            parent_id,
            ..
        } = &records[1]
        else {
            panic!("expected msg record");
        };
        assert_eq!(*parent_id, Some(*first));
        assert_ne!(second, first);
    }

    #[test]
    fn resumed_append_chains_onto_last_record() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        let log = SessionLog::rewrite(dir, &session).unwrap();
        drop(log);

        let mut session = TestSession::load_from(session.id, dir).unwrap();
        let mut log = SessionLog::open(dir, &session).unwrap();
        session.push_message(user_message("second"));
        log.append(&session).unwrap();
        drop(log);

        let log_path = dir.join(session.id.to_string()).join(LOG_FILE_NAME);
        let records: Vec<LogRecord<TestMsg>> = fs::read_to_string(&log_path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let LogRecord::Msg { id: first, .. } = &records[0] else {
            panic!("expected msg record");
        };
        let LogRecord::Msg { parent_id, .. } = &records[1] else {
            panic!("expected msg record");
        };
        assert_eq!(*parent_id, Some(*first));
    }

    #[test]
    fn fork_lineage_fields_are_optional_on_meta() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session = new_session();
        SessionLog::rewrite(dir, &session).unwrap();

        let meta_path = dir.join(session.id.to_string()).join(META_FILE_NAME);
        let raw = fs::read_to_string(&meta_path).unwrap();
        assert!(!raw.contains("parent_session_id"));
        assert!(!raw.contains("created_from_node_id"));

        let mut header: SessionHeader<Value> = serde_json::from_str(&raw).unwrap();
        assert_eq!(header.parent_session_id, None);
        header.parent_session_id = Some("parent-session".into());
        header.created_from_node_id = Some("msg_abc".into());
        let roundtripped: SessionHeader<Value> =
            serde_json::from_str(&serde_json::to_string(&header).unwrap()).unwrap();
        assert_eq!(
            roundtripped.parent_session_id.as_deref(),
            Some("parent-session")
        );
        assert_eq!(
            roundtripped.created_from_node_id.as_deref(),
            Some("msg_abc")
        );
    }
}
