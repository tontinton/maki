//! Session persistence with append-only JSONL log format.
//!
//! Each session is stored as `{uuid}.jsonl`, one JSON record per line. The format is
//! crash-safe: on load, any trailing run of unparseable lines is discarded (a partial
//! flush may corrupt multiple trailing records). `SessionLog` tracks cursor state to
//! enable O(delta) incremental saves.
//!
//! Legacy `.json` files are loaded transparently and converted to `.jsonl` on next save.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use tracing::{info, warn};

use crate::id::{MakiId, MakiIdParseError};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{StateDir, StorageError, atomic_write, now_epoch};

const SESSION_VERSION: u32 = 1;
const LOG_FORMAT_VERSION: u32 = 2;
pub const SESSIONS_DIR: &str = "sessions";
const CWD_INDEX_FILE: &str = "cwd_latest.json";
const CWD_INDEX_STEM: &str = "cwd_latest";
const SCAN_CACHE_FILE: &str = "scan_cache.json";
const SCAN_CACHE_STEM: &str = "scan_cache";
const NON_SESSION_STEMS: [&str; 2] = [CWD_INDEX_STEM, SCAN_CACHE_STEM];
const DEFAULT_TITLE: &str = "New session";
const MAX_TITLE_LEN: usize = 60;
const EPOCH_CHANGED: &str = "messages were rewritten";
const FILE_CHANGED_UNDERNEATH: &str = "file changed underneath";
const CURSOR_AHEAD: &str = "cursor ahead of session";
const LOG_BLOATED: &str = "too many stale meta records";
/// Every append leaves a whole meta record behind and only the last one is ever
/// read. Past this many, the log is rewritten and they all go away at once.
const MAX_APPENDS: usize = 512;
/// Where a shrink rewrite parks the log it is about to drop, as `archive/<id>/`.
const ARCHIVE_DIR: &str = "archive";
/// Archives kept per session. The extra ones go on the next archive, not on a
/// timer.
const ARCHIVE_KEEP: usize = 3;
/// Three copies of a log full of tool output add up fast, so the bytes get a
/// budget of their own. The newest archive always survives, whatever it weighs.
const ARCHIVE_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// A `msg` line starts with this. Matching the prefix beats parsing the log.
const MSG_PREFIX: &[u8] = br#"{"t":"msg""#;

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
}

/// Per-model token breakdown entry. Mirrors the four usage counters tracked by
/// the active provider; kept storage-local to avoid a circular dependency on
/// `maki-providers`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StoredTokenUsage {
    #[serde(default)]
    pub input: u32,
    #[serde(default)]
    pub output: u32,
    #[serde(default)]
    pub cache_creation: u32,
    #[serde(default)]
    pub cache_read: u32,
    /// What the turns billed, in USD. Prices move (some providers by the hour),
    /// so re-pricing these counters later would be fiction. `None` on unpriced
    /// models, and on entries written before we recorded it until the next load
    /// settles an estimate into them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

impl StoredTokenUsage {
    pub fn total_input(&self) -> u32 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_creation)
    }

    pub fn total(&self) -> u32 {
        self.total_input().saturating_add(self.output)
    }
}

impl std::ops::AddAssign for StoredTokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input = self.input.saturating_add(rhs.input);
        self.output = self.output.saturating_add(rhs.output);
        self.cache_creation = self.cache_creation.saturating_add(rhs.cache_creation);
        self.cache_read = self.cache_read.saturating_add(rhs.cache_read);
        add_cost(&mut self.cost, rhs.cost);
    }
}

/// The one way costs are summed, re-exported by `maki-providers` so every
/// running total agrees: `None` until the first priced turn shows up, and from
/// there it only grows.
pub fn add_cost(total: &mut Option<f64>, addend: Option<f64>) {
    if let Some(addend) = addend {
        *total = Some(total.unwrap_or_default() + addend);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default)]
    pub mode: Option<StoredMode>,
    #[serde(default)]
    pub plan_path: Option<String>,
    #[serde(default)]
    pub plan_written: bool,
    #[serde(default)]
    pub session_rules: Vec<StoredRule>,
    #[serde(default)]
    pub context_size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_draft: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_messages: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<StoredThinking>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub workflow: bool,
    /// `None` when the user never set yolo for this session, which is what
    /// makes `--yolo` a property of the invocation rather than of the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo: Option<bool>,
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
    /// Bumped when this session rewrites a collection in place (replaced
    /// messages, tool outputs or subagent histories). Kept apart from
    /// `epoch` so `set_history` adopting a producer's snapshot can never
    /// erase a locally minted void: cursor validity is the pair.
    #[serde(skip)]
    rewrites: u64,
}

#[derive(Serialize)]
pub struct SessionSummary {
    pub id: MakiId,
    pub title: String,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoredEffect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoredMode {
    Build,
    Plan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRule {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub effect: StoredEffect,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ThinkingParseError {
    #[error(
        "unknown thinking value {0:?} (use off, adaptive, minimal, low, medium, high, xhigh, max, or a token budget)"
    )]
    Unknown(String),
    #[error("thinking budget must be greater than zero")]
    BudgetZero,
}

/// Floor for every token budget sent to a provider; some APIs reject smaller values.
pub const MIN_THINKING_BUDGET: u32 = 1024;

/// Thinking effort level. Declaration order is intensity order: the `Ord`
/// derive and [`Effort::ALL`] rely on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    pub const ALL: [Self; 6] = [
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Percentage of the model's max thinking budget this level spends.
    pub const fn percent(self) -> u32 {
        match self {
            Self::Minimal => 10,
            Self::Low => 20,
            Self::Medium => 40,
            Self::High => 60,
            Self::XHigh => 80,
            Self::Max => 100,
        }
    }

    /// `percent` of `max`, clamped to `[MIN_THINKING_BUDGET, max]`.
    /// A `max` below the floor is raised to it.
    pub fn budget(self, max: u32) -> u32 {
        let max = max.max(MIN_THINKING_BUDGET);
        let tokens = (u64::from(max) * u64::from(self.percent()) / 100) as u32;
        tokens.clamp(MIN_THINKING_BUDGET, max)
    }

    /// Inverse of [`Self::budget`]: the lowest level whose percentage covers
    /// `n` tokens out of `max`. Budgets at or above `max` map to `Max`.
    pub fn from_budget(n: u32, max: u32) -> Self {
        let pct = u64::from(n).saturating_mul(100) / u64::from(max.max(1));
        Self::ALL
            .into_iter()
            .find(|e| u64::from(e.percent()) >= pct)
            .unwrap_or(Self::Max)
    }

    /// Nearest level a provider accepts: exact match keeps `self`, otherwise
    /// the closest lower supported level, otherwise the lowest supported.
    /// An empty `supported` list returns `self` unchanged (dynamic model
    /// listings may not declare supported efforts).
    pub fn snap(self, supported: &[Self]) -> Self {
        if supported.is_empty() || supported.contains(&self) {
            return self;
        }
        supported
            .iter()
            .rev()
            .find(|&&e| e < self)
            .copied()
            .unwrap_or(supported[0])
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Effort {
    type Err = ThinkingParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|e| e.as_str() == s)
            .ok_or_else(|| ThinkingParseError::Unknown(s.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum StoredThinking {
    Off,
    Adaptive,
    Effort { level: Effort },
    Budget { tokens: u32 },
}

impl StoredThinking {
    /// The one string-to-thinking parser: `/thinking`, `always_thinking`
    /// config, and the Lua agent API all delegate here.
    pub fn parse_setting(input: &str) -> Result<Self, ThinkingParseError> {
        match input.trim() {
            "off" => Ok(Self::Off),
            "adaptive" => Ok(Self::Adaptive),
            other => {
                if let Ok(level) = other.parse::<Effort>() {
                    return Ok(Self::Effort { level });
                }
                match other.parse::<u32>() {
                    Ok(0) => Err(ThinkingParseError::BudgetZero),
                    Ok(n) => Ok(Self::Budget { tokens: n }),
                    Err(_) => Err(ThinkingParseError::Unknown(other.to_string())),
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSubagent {
    pub tool_use_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Deserialize)]
struct LegacyHeader {
    version: u32,
    id: MakiId,
    title: String,
    cwd: String,
    updated_at: u64,
}

pub trait TitleSource {
    fn first_user_text(&self) -> Option<&str>;
}

/// A pasted code block bakes `\n` into a title and skews width-based padding
/// in single-line UI like the picker, so every title entry point calls this.
pub fn normalize_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn generate_title<M: TitleSource>(messages: &[M]) -> String {
    let first_user_text = messages.iter().find_map(|m| m.first_user_text());

    let Some(text) = first_user_text.map(str::trim).filter(|t| !t.is_empty()) else {
        return DEFAULT_TITLE.into();
    };
    let text = normalize_title(text);

    if text.len() <= MAX_TITLE_LEN {
        return text;
    }

    let boundary = text.floor_char_boundary(MAX_TITLE_LEN);
    let truncated = &text[..boundary];
    match truncated.rfind(' ') {
        Some(pos) if pos > MAX_TITLE_LEN / 2 => format!("{}…", &truncated[..pos]),
        _ => format!("{truncated}…"),
    }
}

// -- JSONL record types --

#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
enum LogRecord<M, U, T> {
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

// -- SessionLog: append-only persistence --

pub struct SessionLog {
    session_id: MakiId,
    file: File,
    /// The session's `(epoch, rewrites)` at the last write. Appending is
    /// sound only while both stay the same.
    saved_epoch: u64,
    saved_rewrites: u64,
    /// Length of the file after the last write. Anything else means someone
    /// truncated, deleted or wrote it, and an append would corrupt it.
    saved_len: u64,
    saved_msg_count: usize,
    appends: usize,
    saved_tool_ids: HashSet<String>,
    saved_sub_msg_counts: HashMap<String, usize>,
    /// Serialized trailing meta record; lets `append` persist meta-only
    /// changes (title, draft, updated_at) instead of dropping them.
    saved_meta: Vec<u8>,
}

fn sub_msg_snapshot<M>(map: &HashMap<String, Arc<Vec<M>>>) -> HashMap<String, usize> {
    map.iter().map(|(k, v)| (k.clone(), v.len())).collect()
}

/// Compaction and rewind hand the log a shorter message list, and writing that
/// out would take the dropped turns with it, so the old file gets a second name
/// under `archive/<id>/` first. Its own path is untouched until the rename, so
/// [`SessionLog::rewrite`] keeps its crash-safety promise.
fn archive_if_shrinking<M, U, T>(dir: &Path, session: &Session<M, U, T>) {
    let path = jsonl_path(dir, session.id);
    let new_count = session.messages.len();
    if !log_msg_count_exceeds(&path, new_count) {
        return;
    }

    let archive_dir = dir.join(ARCHIVE_DIR).join(session.id.to_string());
    if let Err(e) = fs::create_dir_all(&archive_dir) {
        warn!(error = %e, session_id = %session.id, "cannot create session archive dir");
        return;
    }
    let existing = archives_newest_first(&archive_dir);
    let next = existing.first().map_or(0, |a| a.seq) + 1;
    let archive_path = archive_dir.join(format!("{next}.jsonl"));
    let bytes = match link_archive(&path, &archive_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!(
                error = %e,
                session_id = %session.id,
                "cannot archive session log before shrink rewrite"
            );
            return;
        }
    };
    prune_archives(existing, bytes);
    // The live log is about to be renamed away. If the archive's directory
    // entry is not durable by then, a crash frees the only inode holding the
    // dropped turns, which is the loss this whole function exists to prevent.
    crate::sync_parent_dir(&archive_path);
    info!(
        session_id = %session.id,
        new_msgs = new_count,
        archive = %archive_path.display(),
        "archived session log before shrink rewrite"
    );
}

/// Whether the log holds more than `limit` messages, which is the whole
/// question a shrink asks, so the scan stops at the first line past it. A
/// growing log never trips that, so this still walks to EOF once per rewrite:
/// it reads bytes into one reused buffer rather than allocating and
/// UTF-8-validating a `String` per line, which is what made the pass show up
/// next to the write it rides along with. A missing or unreadable file answers
/// no and the rewrite goes ahead as before: nobody should lose a save because
/// the old file would not count.
fn log_msg_count_exceeds(path: &Path, limit: usize) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut count = 0;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {}
        }
        if line.starts_with(MSG_PREFIX) {
            count += 1;
            if count > limit {
                return true;
            }
        }
    }
}

/// The archive is a second name for the log's current inode. The rewrite
/// renames a fresh file over the path, so the old bytes stay whole under the
/// new name and not one of them is copied. Filesystems with no links (FAT32 on
/// a stick) fall back to a plain copy. The size is for the byte budget.
fn link_archive(from: &Path, to: &Path) -> Result<u64, std::io::Error> {
    if fs::hard_link(from, to).is_err() {
        // `create_new` first: the name is only free if the seq scan saw every
        // archive, and `fs::copy` would truncate the one it collided with.
        OpenOptions::new().write(true).create_new(true).open(to)?;
        fs::copy(from, to).inspect_err(|_| {
            let _ = fs::remove_file(to);
        })?;
    }
    Ok(fs::metadata(to)?.len())
}

/// `<seq>.jsonl`, counting up. A number cannot step back the way a clock does
/// after an NTP fix or a suspend, so the order is always the truth and pruning
/// can never mistake the newest archive for the oldest. The mtime says when.
struct Archive {
    seq: u64,
    size: u64,
    path: PathBuf,
}

/// Newest first: the next name comes off the front, pruning walks to the back.
fn archives_newest_first(archive_dir: &Path) -> Vec<Archive> {
    let Ok(entries) = fs::read_dir(archive_dir) else {
        return Vec::new();
    };
    let mut archives: Vec<Archive> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !is_jsonl(&path) {
                return None;
            }
            Some(Archive {
                seq: path.file_stem()?.to_str()?.parse().ok()?,
                size: entry.metadata().ok()?.len(),
                path,
            })
        })
        .collect();
    archives.sort_unstable_by_key(|a| Reverse(a.seq));
    archives
}

/// Walks from the newest and keeps what both budgets allow, so the rest go.
/// `new_bytes` is the archive we just made: it is not in `existing`, so it can
/// never be the one dropped.
fn prune_archives(existing: Vec<Archive>, new_bytes: u64) {
    let mut total = new_bytes;
    let mut room = ARCHIVE_KEEP.saturating_sub(1);
    for archive in existing {
        total += archive.size;
        if room > 0 && total <= ARCHIVE_MAX_BYTES {
            room -= 1;
            continue;
        }
        let _ = fs::remove_file(&archive.path);
    }
}

impl SessionLog {
    /// Starts the file over: writes the whole log through a rename, so a crash
    /// mid-write leaves the old one intact, then claims the cwd index and
    /// sweeps pre-jsonl leftovers. A rewrite that drops messages (compaction,
    /// rewind) parks the previous file under `archive/<id>/` first, keeping the
    /// newest [`ARCHIVE_KEEP`] of them within [`ARCHIVE_MAX_BYTES`]. The only
    /// way to get a usable cursor onto a file this process did not write: a
    /// cursor read back from disk describes the session that was loaded, never
    /// the live one.
    pub fn rewrite<M, U, T>(dir: &Path, session: &Session<M, U, T>) -> Result<Self, SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        let log = Self::write_canonical(dir, session)?;
        update_cwd_index(dir, &session.cwd, session.id)?;
        Ok(log)
    }

    /// [`Self::rewrite`] without claiming the cwd index: migrating a legacy
    /// file on load must not make that session the cwd's latest.
    fn write_canonical<M, U, T>(
        dir: &Path,
        session: &Session<M, U, T>,
    ) -> Result<Self, SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        fs::create_dir_all(dir).map_err(StorageError::from)?;
        let path = jsonl_path(dir, session.id);
        let tmp = path.with_extension("jsonl.tmp");

        let mut tmp_file = File::create(&tmp).map_err(StorageError::from)?;
        write_full_session(&mut tmp_file, session)?;
        tmp_file.sync_data().map_err(StorageError::from)?;
        // Last thing before the rename: a write that never lands must not
        // spend an archive slot, since taking one prunes the oldest.
        archive_if_shrinking(dir, session);
        fs::rename(&tmp, &path).map_err(StorageError::from)?;
        crate::sync_parent_dir(&path);

        if let Err(e) = remove_legacy_files(dir, session.id) {
            warn!(error = %e, "legacy session files remain after rewrite");
        }
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(StorageError::from)?;
        Ok(Self::cursor_from(session, file))
    }

    pub fn session_id(&self) -> MakiId {
        self.session_id
    }

    pub fn append<M, U, T>(&mut self, session: &Session<M, U, T>) -> Result<(), SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        self.require_same_id(session)?;
        self.ensure_appendable(session)?;

        let mut buf = Vec::new();
        let mut new_msg_count = self.saved_msg_count;
        let mut new_tool_ids = Vec::new();

        for msg in &session.messages[self.saved_msg_count..] {
            append_record(&mut buf, &LogRecord::<&M, &U, &T>::Msg { d: msg })?;
            new_msg_count += 1;
        }

        for (id, output) in &session.tool_outputs {
            if !self.saved_tool_ids.contains(id) {
                append_record(
                    &mut buf,
                    &LogRecord::<&M, &U, &T>::Out {
                        id: id.clone(),
                        d: output,
                    },
                )?;
                new_tool_ids.push(id.clone());
            }
        }

        let mut new_sub_counts: Vec<(String, usize)> = Vec::new();
        for (sub_id, msgs) in &session.subagent_messages {
            let saved = self.saved_sub_msg_counts.get(sub_id).copied().unwrap_or(0);
            for msg in &msgs[saved..] {
                append_record(
                    &mut buf,
                    &LogRecord::<&M, &U, &T>::SubMsg {
                        sub: sub_id.clone(),
                        d: msg,
                    },
                )?;
            }
            if msgs.len() > saved {
                new_sub_counts.push((sub_id.clone(), msgs.len()));
            }
        }

        let meta = meta_record(session)?;
        if buf.is_empty() && meta == self.saved_meta {
            return Ok(());
        }
        buf.extend_from_slice(&meta);

        if let Err(e) = self
            .file
            .write_all(&buf)
            .and_then(|()| self.file.sync_data())
        {
            // A failed write can leave partial bytes; roll back to the last
            // record boundary so the file matches the unadvanced cursors and
            // a retry appends cleanly instead of duplicating records.
            let _ = self.file.set_len(self.saved_len);
            return Err(StorageError::from(e).into());
        }

        self.saved_len += buf.len() as u64;
        self.appends += 1;
        self.saved_msg_count = new_msg_count;
        self.saved_tool_ids.extend(new_tool_ids);
        for (sub_id, count) in new_sub_counts {
            self.saved_sub_msg_counts.insert(sub_id, count);
        }
        self.saved_meta = meta;

        Ok(())
    }

    fn cursor_from<M, U, T>(session: &Session<M, U, T>, file: File) -> Self
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        let saved_len = file.metadata().map(|m| m.len()).unwrap_or_default();
        Self {
            session_id: session.id,
            file,
            saved_epoch: session.epoch,
            saved_rewrites: session.rewrites,
            saved_len,
            saved_msg_count: session.messages.len(),
            appends: 0,
            saved_tool_ids: session.tool_outputs.keys().cloned().collect(),
            saved_sub_msg_counts: sub_msg_snapshot(&session.subagent_messages),
            saved_meta: meta_record(session).unwrap_or_default(),
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
        let reason = if session.epoch != self.saved_epoch || session.rewrites != self.saved_rewrites
        {
            EPOCH_CHANGED
        } else if self.file.metadata().map_err(StorageError::from)?.len() != self.saved_len {
            FILE_CHANGED_UNDERNEATH
        } else if self.appends >= MAX_APPENDS {
            LOG_BLOATED
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
}

fn meta_record<M, U, T>(session: &Session<M, U, T>) -> Result<Vec<u8>, SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    let mut buf = Vec::new();
    append_record(
        &mut buf,
        &LogRecord::<&M, &U, &T>::Meta {
            title: session.title.clone(),
            token_usage: &session.token_usage,
            updated_at: session.updated_at,
            subagents: session.subagents.clone(),
            usage_by_model: session.usage_by_model.clone(),
            meta: session.meta.clone(),
        },
    )?;
    Ok(buf)
}

fn write_full_session<M, U, T>(
    file: &mut File,
    session: &Session<M, U, T>,
) -> Result<(), SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    let mut buf = Vec::new();
    append_record(
        &mut buf,
        &LogRecord::<&M, &U, &T>::Header {
            v: LOG_FORMAT_VERSION,
            id: session.id,
            model: session.model.clone(),
            cwd: session.cwd.clone(),
            created_at: session.created_at,
        },
    )?;
    for msg in session.messages.iter() {
        append_record(&mut buf, &LogRecord::<&M, &U, &T>::Msg { d: msg })?;
    }
    for (id, output) in &session.tool_outputs {
        append_record(
            &mut buf,
            &LogRecord::<&M, &U, &T>::Out {
                id: id.clone(),
                d: output,
            },
        )?;
    }
    for (sub_id, msgs) in &session.subagent_messages {
        for msg in msgs.iter() {
            append_record(
                &mut buf,
                &LogRecord::<&M, &U, &T>::SubMsg {
                    sub: sub_id.clone(),
                    d: msg,
                },
            )?;
        }
    }
    buf.extend_from_slice(&meta_record(session)?);
    file.write_all(&buf).map_err(StorageError::from)?;
    Ok(())
}

fn append_record<R: Serialize>(buf: &mut Vec<u8>, record: &R) -> Result<(), SessionError> {
    serde_json::to_writer(&mut *buf, record).map_err(StorageError::from)?;
    buf.push(b'\n');
    Ok(())
}

/// Tag-only probe used to classify a line that failed the strict `LogRecord`
/// parse: distinguishes a header with a bad id from a genuinely unknown record.
#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum RawTag {
    Header {
        id: String,
    },
    #[serde(other)]
    Other,
}

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
        let record: LogRecord<M, U, T> = match serde_json::from_slice(line) {
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
            LogRecord::Header {
                v,
                id: h_id,
                model: h_model,
                cwd: h_cwd,
                created_at: h_created,
            } => {
                if v != LOG_FORMAT_VERSION {
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
            LogRecord::Msg { d } => messages.push(d),
            LogRecord::Out { id: out_id, d } => {
                tool_outputs.insert(out_id, Arc::new(d));
            }
            LogRecord::SubMsg { sub, d } => {
                subagent_messages.entry(sub).or_default().push(d);
            }
            LogRecord::Meta {
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

    Ok(Session {
        version: SESSION_VERSION,
        id,
        title,
        cwd,
        model,
        messages: Arc::new(messages),
        token_usage,
        tool_outputs,
        subagent_messages: subagent_messages
            .into_iter()
            .map(|(id, msgs)| (id, Arc::new(msgs)))
            .collect(),
        subagents,
        usage_by_model,
        meta,
        created_at,
        updated_at,
        revision: 0,
        content_revision: 0,
        epoch: next_epoch(),
        rewrites: 0,
    })
}

// -- CWD index --

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

fn jsonl_path(dir: &Path, id: MakiId) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

fn json_path(dir: &Path, id: MakiId) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

fn remove_legacy_files(dir: &Path, id: MakiId) -> Result<bool, SessionError> {
    let mut removed = try_remove(&json_path(dir, id))?;
    for legacy in find_legacy_files(dir, id) {
        removed |= try_remove(&legacy)?;
    }
    Ok(removed)
}

fn try_remove(path: &Path) -> Result<bool, StorageError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
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

#[derive(Deserialize)]
struct JsonlHeader {
    v: u32,
    id: MakiId,
    cwd: String,
}

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

/// Cached scan result for one session file, keyed by file name and validated
/// by (size, mtime): stale entries are rescanned, deleted files pruned.
/// `header: None` marks files that failed to scan (wrong version, foreign
/// format), so they are not re-read on every list either.
#[derive(Serialize, Deserialize)]
struct ScanCacheEntry {
    size: u64,
    mtime_ms: u64,
    header: Option<ScannedHeader>,
}

#[derive(Serialize, Deserialize)]
struct ScannedHeader {
    id: MakiId,
    cwd: String,
    title: String,
    updated_at: u64,
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
    let mut cache = load_scan_cache(dir);
    let mut fresh = ScanCache::new();
    let mut dirty = false;
    let mut out = Vec::new();
    for path in session_entries(dir)? {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((size, mtime_ms)) = file_signature(&path) else {
            continue;
        };
        let entry = match cache.remove(name) {
            Some(e) if e.size == size && e.mtime_ms == mtime_ms => e,
            _ => {
                dirty = true;
                let header = if is_jsonl(&path) {
                    scan_jsonl_header(&path)
                } else {
                    scan_legacy_header(&path)
                };
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

const TAIL_BUF: u64 = 4096;

fn scan_jsonl_header(path: &Path) -> Option<ScannedHeader> {
    let mut file = File::open(path).ok()?;
    let header: JsonlHeader = {
        let mut reader = BufReader::new(&file);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        serde_json::from_str(line.trim_end()).ok()?
    };
    if header.v != LOG_FORMAT_VERSION {
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

fn scan_legacy_header(path: &Path) -> Option<ScannedHeader> {
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

fn session_entries(dir: &Path) -> Result<Vec<PathBuf>, StorageError> {
    Ok(fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|p| is_session_file(p))
        .collect())
}

fn is_session_file(p: &Path) -> bool {
    p.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| !NON_SESSION_STEMS.contains(&s))
        && p.extension().is_some_and(|e| e == "json" || e == "jsonl")
}

fn find_legacy_files(dir: &Path, id: MakiId) -> Vec<PathBuf> {
    let canonical = id.to_string();
    session_entries(dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s != canonical && s.parse::<MakiId>() == Ok(id))
        })
        .collect()
}

fn locate_session_file(dir: &Path, id: MakiId) -> Option<PathBuf> {
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

fn load_session_at<M, U, T>(path: &Path) -> Result<Session<M, U, T>, SessionError>
where
    M: DeserializeOwned,
    U: DeserializeOwned + Default,
    T: DeserializeOwned,
{
    let data = fs::read(path).map_err(StorageError::from)?;
    // Held across both formats: either one decodes the same image payload once
    // per record that mentions it.
    let _intern = crate::intern::Scope::enter();
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

// -- Session impl --

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
            rewrites: 0,
        }
    }

    pub fn messages(&self) -> &[M] {
        &self.messages
    }

    pub fn take_messages(self) -> Vec<M> {
        Arc::unwrap_or_clone(self.messages)
    }

    pub fn tool_outputs(&self) -> &HashMap<String, Arc<T>> {
        &self.tool_outputs
    }

    pub fn subagent_messages(&self) -> &HashMap<String, Arc<Vec<M>>> {
        &self.subagent_messages
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

    /// Every append cursor into the log is void from here on. Counted in
    /// `rewrites`, which snapshot adoption never touches, so a same-frame
    /// `set_history` cannot erase the void before the writer sees it.
    fn rewrite(&mut self) {
        self.rewrites += 1;
        self.touch();
    }

    /// [`Self::rewrite`] for local changes to `messages`: they also leave the
    /// producer's run, so the epoch is minted fresh. Once this state is
    /// saved, re-adopting a stale run snapshot keeps diverging instead of
    /// splicing its tail onto a rewound log.
    fn rewrite_messages(&mut self) {
        self.epoch = next_epoch();
        self.rewrite();
    }

    pub fn push_message(&mut self, msg: M) {
        Arc::make_mut(&mut self.messages).push(msg);
        self.touch();
    }

    pub fn replace_messages(&mut self, messages: Vec<M>) {
        self.messages = Arc::new(messages);
        self.rewrite_messages();
    }

    pub fn truncate_messages(&mut self, len: usize) {
        if len >= self.messages.len() {
            return;
        }
        Arc::make_mut(&mut self.messages).truncate(len);
        self.rewrite_messages();
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
    pub fn insert_tool_output(&mut self, id: String, output: Arc<T>) {
        if self.tool_outputs.insert(id, output).is_some() {
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

    /// Deleting a task drops its transcript from the log, which an append
    /// cannot express, so the cursors are voided like any rewrite. The
    /// `tool_ids` closure extracts tool-use ids from messages so the
    /// subagent's tool outputs are reclaimed from the shared map.
    pub fn remove_subagent(&mut self, id: &str, tool_ids: impl Fn(&M) -> Vec<String>) {
        let removed = self.subagent_messages.remove(id);
        if let Some(msgs) = &removed {
            let stale: HashSet<String> = msgs.iter().flat_map(&tool_ids).collect();
            self.tool_outputs.retain(|tid, _| !stale.contains(tid));
        }
        let len = self.subagents.len();
        self.subagents.retain(|sa| sa.tool_use_id != id);
        if removed.is_some() || self.subagents.len() != len {
            self.rewrite();
        }
    }

    pub fn usage_by_model(&self) -> &HashMap<String, StoredTokenUsage> {
        &self.usage_by_model
    }

    /// For settling costs on load; every other write goes through
    /// [`Self::add_model_usage`].
    pub fn usage_by_model_mut(&mut self) -> &mut HashMap<String, StoredTokenUsage> {
        self.touch();
        &mut self.usage_by_model
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

    pub fn save(&mut self, dir: &StateDir) -> Result<(), SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        self.save_to(&sessions_dir)
    }

    pub fn save_to(&mut self, dir: &Path) -> Result<(), SessionError> {
        self.updated_at = now_epoch();
        SessionLog::rewrite(dir, self)?;
        Ok(())
    }

    pub fn load(id: MakiId, dir: &StateDir) -> Result<Self, SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        Self::load_from(id, &sessions_dir)
    }

    pub fn load_from(id: MakiId, dir: &Path) -> Result<Self, SessionError> {
        let Some(path) = locate_session_file(dir, id) else {
            return Err(StorageError::NotFound(id.to_string()).into());
        };
        let session = load_session_at::<M, U, T>(&path)?;
        if path != jsonl_path(dir, id)
            && let Err(e) = SessionLog::write_canonical(dir, &session)
        {
            warn!(error = %e, "failed migrate to canonical jsonl; keeping legacy file");
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
        let mut removed = try_remove(&jsonl_path(dir, id))?;
        removed |= remove_legacy_files(dir, id)?;
        // Backups, not the session: failing to sweep them must not fail a
        // delete whose log is already gone, and their presence alone does not
        // make a session exist.
        if let Err(e) = fs::remove_dir_all(dir.join(ARCHIVE_DIR).join(id.to_string()))
            && e.kind() != ErrorKind::NotFound
        {
            warn!(error = %e, session_id = %id, "session archives remain after delete");
        }
        if !removed {
            return Err(StorageError::NotFound(id.to_string()).into());
        }
        remove_from_cwd_index(dir, id)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Effort;
    use super::StoredThinking;
    use super::ThinkingParseError;
    use super::{
        ARCHIVE_DIR, ARCHIVE_KEEP, ARCHIVE_MAX_BYTES, CWD_INDEX_FILE, DEFAULT_TITLE, LOG_BLOATED,
        MAX_APPENDS, MAX_TITLE_LEN, MSG_PREFIX, SESSION_VERSION, StoredSubagent, TAIL_BUF,
        generate_title, json_path, jsonl_path, load_cwd_index, next_epoch, update_cwd_index,
        write_full_session,
    };
    use super::{
        HistorySnapshot, SCAN_CACHE_FILE, Session, SessionError, SessionLog, SessionMeta,
        StorageError, TitleSource,
    };
    use crate::id::MakiId;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tempfile::TempDir;
    use test_case::test_case;

    type TestSession = Session<Value, Value, Value>;

    const LEGACY_HEX_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const SONNET_COST: f64 = 0.42;
    const HAIKU_COST: f64 = 0.08;
    const TAMPERED_TITLE: &str = "tampered cached title";
    const PENDING_DRAFT: &str = "half typed thought";
    /// Two of these already break the byte budget.
    const FAKE_ARCHIVE_BYTES: u64 = ARCHIVE_MAX_BYTES / 2;
    const EXISTING_ARCHIVE_SEQ: u64 = 7;

    impl TitleSource for Value {
        fn first_user_text(&self) -> Option<&str> {
            if self.get("role")?.as_str()? != "user" {
                return None;
            }
            self.get("content")?.as_array()?.iter().find_map(|b| {
                if b.get("type")?.as_str()? == "text" {
                    let text = b.get("text")?.as_str()?;
                    (!text.is_empty()).then_some(text)
                } else {
                    None
                }
            })
        }
    }

    fn user_message(text: &str) -> Value {
        text_message("user", text)
    }

    fn assistant_message(text: &str) -> Value {
        text_message("assistant", text)
    }

    fn text_message(role: &str, text: &str) -> Value {
        serde_json::json!({
            "role": role,
            "content": [{"type": "text", "text": text}]
        })
    }

    fn write_legacy_jsonl(path: &Path, session: &TestSession) {
        let mut file = std::fs::File::create(path).unwrap();
        write_full_session(&mut file, session).unwrap();
    }

    fn append_raw_msg(path: &Path, message: Value) {
        let record = serde_json::to_string(&serde_json::json!({"t":"msg","d": message})).unwrap();
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(record.as_bytes()).unwrap();
        file.write_all(b"\n").unwrap();
    }

    #[test]
    fn prune_orphans_drops_unreachable_tool_state() {
        fn ids(m: &Value) -> Vec<String> {
            vec![m.as_str().unwrap().to_owned()]
        }
        fn subagent(id: &str) -> StoredSubagent {
            StoredSubagent {
                tool_use_id: id.into(),
                name: "sub".into(),
                model: None,
            }
        }

        let mut session: TestSession = Session::new("model", "/p");
        session.push_message("task-live".into());
        session
            .subagent_messages
            .insert("task-live".into(), Arc::new(vec!["sub-tool".into()]));
        session
            .subagent_messages
            .insert("task-stale".into(), Arc::new(vec!["stale-sub-tool".into()]));
        session.set_subagents(vec![subagent("task-live"), subagent("task-stale")]);
        for id in ["task-live", "sub-tool", "stale-sub-tool", "orphan"] {
            session.insert_tool_output(id.into(), Arc::new(Value::Null));
        }

        session.prune_orphans(ids);

        assert_eq!(
            session.subagent_messages().keys().collect::<Vec<_>>(),
            ["task-live"]
        );
        let subagent_ids: Vec<_> = session
            .subagents()
            .iter()
            .map(|sa| sa.tool_use_id.as_str())
            .collect();
        assert_eq!(subagent_ids, ["task-live"]);
        let mut outputs: Vec<_> = session.tool_outputs().keys().cloned().collect();
        outputs.sort();
        assert_eq!(outputs, ["sub-tool", "task-live"]);
    }

    #[test]
    fn roundtrip_save_load() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession =
            Session::new("anthropic/claude-sonnet-4", "/home/test/project");
        session.push_message(user_message("hello"));
        session.set_subagent_messages(
            "tool-1".into(),
            vec![user_message("sub-prompt"), assistant_message("sub-reply")],
        );
        session.save_to(dir).unwrap();

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.model, "anthropic/claude-sonnet-4");
        assert_eq!(loaded.cwd, "/home/test/project");
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.version, SESSION_VERSION);
        assert_eq!(loaded.subagent_messages["tool-1"].len(), 2);
    }

    #[test]
    fn roundtrip_usage_by_model() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("anthropic/claude-sonnet-4", "/project");
        session.add_model_usage(
            "claude-sonnet-4",
            super::StoredTokenUsage {
                input: 100,
                output: 20,
                cache_creation: 5,
                cache_read: 40,
                cost: Some(SONNET_COST),
            },
        );
        session.add_model_usage(
            "claude-haiku-4",
            super::StoredTokenUsage {
                input: 30,
                output: 10,
                cost: Some(HAIKU_COST),
                ..Default::default()
            },
        );
        session.save_to(dir).unwrap();

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        let sonnet = &loaded.usage_by_model()["claude-sonnet-4"];
        assert_eq!(sonnet.input, 100);
        assert_eq!(sonnet.output, 20);
        assert_eq!(sonnet.cache_read, 40);
        assert_eq!(sonnet.total_input(), 145);
        assert_eq!(sonnet.cost, Some(SONNET_COST));
        assert_eq!(loaded.usage_by_model()["claude-haiku-4"].total(), 40);
        assert_eq!(
            loaded.usage_by_model()["claude-haiku-4"].cost,
            Some(HAIKU_COST)
        );
    }

    /// A turn that reports no price must not erase what was already billed.
    #[test_case(None, None, None ; "unpriced_stays_unpriced")]
    #[test_case(None, Some(SONNET_COST), Some(SONNET_COST) ; "first_price_starts_the_total")]
    #[test_case(Some(SONNET_COST), Some(HAIKU_COST), Some(SONNET_COST + HAIKU_COST) ; "priced_turns_accumulate")]
    #[test_case(Some(SONNET_COST), None, Some(SONNET_COST) ; "unpriced_turn_keeps_the_total")]
    fn add_cost_only_grows_a_total(
        mut total: Option<f64>,
        addend: Option<f64>,
        expected: Option<f64>,
    ) {
        super::add_cost(&mut total, addend);
        assert_eq!(total, expected);
    }

    fn usage(input: u32, cost: Option<f64>) -> super::StoredTokenUsage {
        super::StoredTokenUsage {
            input,
            cost,
            ..Default::default()
        }
    }

    fn saved_usage_by_model(dir: &Path, id: MakiId) -> Value {
        let text = fs::read_to_string(jsonl_path(dir, id)).unwrap();
        let meta = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|record| record["t"] == "meta")
            .expect("saved session has a meta record");
        meta["usage_by_model"].clone()
    }

    /// Session files predate `cost`, so an entry without the key loads unpriced
    /// with its counters intact, and saving it back must not mint one: older
    /// builds still read these files.
    #[test]
    fn legacy_usage_entry_loads_unpriced_and_stays_that_way_on_disk() {
        let id: MakiId = LEGACY_HEX_ID.parse().unwrap();
        let json = format!(
            r#"{{"t":"header","v":2,"id":"{LEGACY_HEX_ID}","model":"m","cwd":"/","created_at":0}}
{{"t":"meta","title":"t","token_usage":null,"updated_at":0,"usage_by_model":{{"m":{{"input":7,"output":3}}}}}}"#
        );
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(format!("{LEGACY_HEX_ID}.jsonl")), json).unwrap();

        let mut loaded = TestSession::load_from(id, tmp.path()).unwrap();
        let entry = loaded.usage_by_model()["m"];
        assert_eq!(entry.cost, None, "no key means unpriced, not free");
        assert_eq!((entry.input, entry.output), (7, 3));

        let dir = tmp.path().join("rewritten");
        fs::create_dir(&dir).unwrap();
        loaded.save_to(&dir).unwrap();
        assert!(
            saved_usage_by_model(&dir, id)["m"].get("cost").is_none(),
            "an unpriced entry writes no cost key"
        );
    }

    /// What a turn billed is written verbatim, read back verbatim, and keeps
    /// adding up after a reload. A later unpriced turn must not throw away what
    /// the earlier ones paid.
    #[test]
    fn recorded_costs_survive_a_reload_and_keep_adding_up() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("anthropic/claude-sonnet-4", "/project");
        session.add_model_usage("claude-sonnet-4", usage(100, Some(SONNET_COST)));
        session.add_model_usage("claude-haiku-4", usage(30, None));
        session.save_to(dir).unwrap();

        let on_disk = saved_usage_by_model(dir, session.id);
        assert_eq!(on_disk["claude-sonnet-4"]["cost"], Value::from(SONNET_COST));

        let mut loaded = TestSession::load_from(session.id, dir).unwrap();
        loaded.add_model_usage("claude-sonnet-4", usage(50, None));
        loaded.add_model_usage("claude-haiku-4", usage(10, Some(HAIKU_COST)));

        let sonnet = loaded.usage_by_model()["claude-sonnet-4"];
        assert_eq!((sonnet.input, sonnet.cost), (150, Some(SONNET_COST)));
        let haiku = loaded.usage_by_model()["claude-haiku-4"];
        assert_eq!((haiku.input, haiku.cost), (40, Some(HAIKU_COST)));
    }

    #[test]
    fn usage_by_model_absent_on_legacy_session() {
        let id: MakiId = LEGACY_HEX_ID.parse().unwrap();
        let json = format!(
            r#"{{"t":"header","v":2,"id":"{LEGACY_HEX_ID}","model":"m","cwd":"/","created_at":0}}
{{"t":"meta","title":"t","token_usage":null,"updated_at":0}}"#
        );
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(format!("{LEGACY_HEX_ID}.jsonl"));
        fs::write(&path, json).unwrap();
        let loaded = TestSession::load_from(id, tmp.path()).unwrap();
        assert!(loaded.usage_by_model().is_empty());
    }

    /// `subagents` and `usage_by_model` moved off `SessionMeta` onto the
    /// session, which must not move them in the file: they were flattened into
    /// the meta record and they still sit there.
    #[test]
    fn session_owned_fields_keep_their_place_in_the_meta_record() {
        let id: MakiId = LEGACY_HEX_ID.parse().unwrap();
        let meta_line = concat!(
            r#"{"t":"meta","title":"t","token_usage":null,"updated_at":0,"fast":true,"#,
            r#""subagents":[{"tool_use_id":"t1","name":"child"}],"#,
            r#""usage_by_model":{"m":{"input":7,"output":3}}}"#,
        );
        let json = format!(
            r#"{{"t":"header","v":2,"id":"{LEGACY_HEX_ID}","model":"m","cwd":"/","created_at":0}}
{meta_line}"#
        );
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(format!("{LEGACY_HEX_ID}.jsonl")), json).unwrap();

        let mut loaded = TestSession::load_from(id, tmp.path()).unwrap();
        assert_eq!(loaded.subagents()[0].name, "child");
        assert_eq!(loaded.usage_by_model()["m"].total(), 10);
        assert!(loaded.meta.fast, "flattened meta still parses alongside");

        let dir = tmp.path().join("rewritten");
        fs::create_dir(&dir).unwrap();
        loaded.save_to(&dir).unwrap();
        let reloaded = TestSession::load_from(id, &dir).unwrap();
        assert_same_session(&reloaded, &loaded);
        assert_eq!(reloaded.subagents(), loaded.subagents());
        assert_eq!(reloaded.usage_by_model(), loaded.usage_by_model());
    }

    #[test]
    fn roundtrip_jsonl_incremental() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("first"));

        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        session.push_message(assistant_message("reply"));
        session.push_message(user_message("second"));
        session.tool_outputs.insert(
            "tool-1".into(),
            Arc::new(serde_json::json!({"result": "ok"})),
        );
        session
            .subagent_messages
            .insert("sub-1".into(), Arc::new(vec![user_message("sub-prompt")]));
        log.append(&session).unwrap();

        Arc::make_mut(session.subagent_messages.get_mut("sub-1").unwrap())
            .push(assistant_message("sub-reply"));
        session
            .subagent_messages
            .insert("sub-2".into(), Arc::new(vec![user_message("sub-2-prompt")]));
        log.append(&session).unwrap();

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(loaded.messages().len(), 3);
        assert_eq!(loaded.tool_outputs().len(), 1);
        assert!(loaded.tool_outputs().contains_key("tool-1"));
        assert_eq!(loaded.subagent_messages["sub-1"].len(), 2);
        assert_eq!(loaded.subagent_messages["sub-2"].len(), 1);
    }

    /// The mirror re-adopts the run's snapshot on every checkpoint; that must
    /// not erase the void minted by a same-frame in-place replacement, or the
    /// writer appends onto a stale prefix and persists a mixed transcript.
    #[test]
    fn snapshot_adoption_does_not_erase_a_local_rewrite() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: Arc<TestSession> = Arc::new(Session::new("m", "/project"));
        let run = HistorySnapshot {
            epoch: next_epoch(),
            messages: Arc::new(vec![user_message("hi")]),
        };
        let meta = session.meta.clone();
        Session::checkpoint(&mut session, Some(&run), meta.clone(), Value::Null);
        Arc::make_mut(&mut session)
            .set_subagent_messages("sub-1".into(), vec![user_message("old")]);
        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        Arc::make_mut(&mut session)
            .set_subagent_messages("sub-1".into(), vec![user_message("new")]);
        let advanced = HistorySnapshot {
            epoch: run.epoch,
            messages: Arc::new(vec![user_message("hi"), assistant_message("reply")]),
        };
        Session::checkpoint(&mut session, Some(&advanced), meta, Value::Null);
        write_through(&mut log, dir, &session);

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_same_session(&loaded, &session);
    }

    #[test]
    fn replacing_subagent_messages_rewrites_the_log() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        session.set_subagent_messages("sub-1".into(), vec![user_message("old")]);
        write_through(&mut log, dir, &session);

        session.set_subagent_messages("sub-1".into(), vec![user_message("new")]);
        write_through(&mut log, dir, &session);

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_same_session(&loaded, &session);
    }

    #[test]
    fn log_asks_for_a_rewrite_once_appends_pile_up() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        for i in 0..MAX_APPENDS {
            session.push_message(user_message(&format!("m{i}")));
            log.append(&session).unwrap();
        }
        session.push_message(user_message("one too many"));

        assert!(matches!(
            log.append(&session),
            Err(SessionError::LogDiverged {
                reason: LOG_BLOATED
            })
        ));
    }

    /// `cwd` and `model` live in the header record, so changing them must
    /// diverge the log into a rewrite, which also refreshes the cwd index.
    #[test]
    fn cwd_and_model_changes_survive_reload_through_a_rewrite() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/old");
        session.push_message(user_message("hi"));
        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        session.set_model("m2".into());
        session.set_cwd("/new".into());
        assert!(matches!(
            log.append(&session),
            Err(SessionError::LogDiverged { .. })
        ));
        drop(SessionLog::rewrite(dir, &session).unwrap());

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(loaded.model, "m2");
        assert_eq!(loaded.cwd, "/new");
        assert_eq!(
            load_cwd_index(dir).get("/new"),
            Some(&session.id.to_string())
        );
    }

    #[test]
    fn append_wrong_session_returns_id_mismatch() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session_a: TestSession = Session::new("m", "/project");
        let session_b: TestSession = Session::new("m", "/project");
        let mut log = SessionLog::rewrite(dir, &session_a).unwrap();

        let err = log.append(&session_b).unwrap_err();
        assert!(matches!(err, SessionError::IdMismatch { .. }));
    }

    #[test]
    fn crash_recovery_truncated_line() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("survives"));
        session.save_to(dir).unwrap();

        let path = jsonl_path(dir, session.id);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"t\":\"msg\",\"d\":{\"trun").unwrap();

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(loaded.messages().len(), 1);
    }

    #[test]
    fn rewind_compact() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        for i in 0..10 {
            session.push_message(user_message(&format!("msg-{i}")));
        }
        session.set_subagent_messages(
            "sub-1".into(),
            vec![user_message("sub-prompt"), assistant_message("sub-reply")],
        );
        drop(SessionLog::rewrite(dir, &session).unwrap());

        session.truncate_messages(5);
        session.tool_outputs.clear();
        session.subagent_messages.remove("sub-1");
        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        session.push_message(user_message("after-compact-1"));
        session.push_message(user_message("after-compact-2"));
        session.push_message(user_message("after-compact-3"));
        log.append(&session).unwrap();

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(loaded.messages().len(), 8);
        assert!(loaded.subagent_messages().is_empty());
    }

    fn archive_dir_for(dir: &Path, id: MakiId) -> PathBuf {
        dir.join(ARCHIVE_DIR).join(id.to_string())
    }

    fn archive_paths(dir: &Path, id: MakiId) -> Vec<PathBuf> {
        let mut paths = fs::read_dir(archive_dir_for(dir, id))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    fn msg_line_count(path: &Path) -> usize {
        fs::read(path)
            .unwrap()
            .split(|&b| b == b'\n')
            .filter(|line| line.starts_with(MSG_PREFIX))
            .count()
    }

    #[test]
    fn rewrite_dropping_messages_archives_the_old_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("model", "/p");
        for i in 0..5 {
            session.push_message(user_message(&format!("turn {i}")));
        }
        session.save_to(dir).unwrap();

        session.replace_messages(vec![user_message("summary")]);
        session.save_to(dir).unwrap();

        let live = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(live.messages().len(), 1);
        let archives = archive_paths(dir, session.id);
        assert_eq!(archives.len(), 1);
        assert_eq!(msg_line_count(&archives[0]), 5);
    }

    #[test]
    fn rewrite_without_shrink_does_not_archive() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("model", "/p");
        session.push_message(user_message("one"));
        session.save_to(dir).unwrap();

        session.push_message(user_message("two"));
        session.save_to(dir).unwrap();

        assert!(!archive_dir_for(dir, session.id).exists());
    }

    #[test]
    fn archived_file_round_trips() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("model", "/p");
        let pre: Vec<Value> = (0..3).map(|i| user_message(&format!("turn {i}"))).collect();
        for msg in &pre {
            session.push_message(msg.clone());
        }
        session.save_to(dir).unwrap();

        session.replace_messages(vec![assistant_message("summary")]);
        session.save_to(dir).unwrap();

        let scratch = TempDir::new().unwrap();
        let archives = archive_paths(dir, session.id);
        assert_eq!(archives.len(), 1);
        fs::copy(
            &archives[0],
            scratch.path().join(format!("{}.jsonl", session.id)),
        )
        .unwrap();
        let archived = TestSession::load_from(session.id, scratch.path()).unwrap();
        assert_eq!(archived.messages(), pre.as_slice());
    }

    #[test]
    fn archive_retention_keeps_newest_three() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("model", "/p");
        session.push_message(user_message("seed"));
        session.save_to(dir).unwrap();
        for round in 1..=5 {
            for _ in 0..round {
                session.push_message(user_message(&format!("turn {round}")));
            }
            session.save_to(dir).unwrap();
            session.replace_messages(vec![user_message(&format!("summary {round}"))]);
            session.save_to(dir).unwrap();
        }

        let archives = archive_paths(dir, session.id);
        assert_eq!(archives.len(), ARCHIVE_KEEP);
        let mut msg_counts: Vec<usize> = archives.iter().map(|p| msg_line_count(p)).collect();
        msg_counts.sort_unstable();
        assert_eq!(msg_counts, [4, 5, 6]);
    }

    /// A new name has to beat every name already there, or pruning would read
    /// the fresh archive as the oldest and eat it.
    #[test]
    fn archive_names_count_up_from_the_newest() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("model", "/p");
        session.push_message(user_message("one"));
        session.push_message(user_message("two"));
        session.save_to(dir).unwrap();

        let archive_dir = archive_dir_for(dir, session.id);
        fs::create_dir_all(&archive_dir).unwrap();
        let existing = archive_dir.join(format!("{EXISTING_ARCHIVE_SEQ}.jsonl"));
        fs::write(&existing, "").unwrap();

        session.replace_messages(vec![user_message("summary")]);
        session.save_to(dir).unwrap();

        let fresh = archive_dir.join(format!("{}.jsonl", EXISTING_ARCHIVE_SEQ + 1));
        assert_eq!(
            archive_paths(dir, session.id),
            vec![existing, fresh.clone()]
        );
        assert_eq!(msg_line_count(&fresh), 2);
    }

    #[test]
    fn archive_retention_honors_the_byte_budget() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("model", "/p");
        session.push_message(user_message("one"));
        session.push_message(user_message("two"));
        session.save_to(dir).unwrap();

        let archive_dir = archive_dir_for(dir, session.id);
        fs::create_dir_all(&archive_dir).unwrap();
        let fakes: Vec<PathBuf> = (1..=3)
            .map(|ms| {
                let path = archive_dir.join(format!("{ms}.jsonl"));
                // Sparse: the length is all the budget looks at.
                fs::File::create(&path)
                    .unwrap()
                    .set_len(FAKE_ARCHIVE_BYTES)
                    .unwrap();
                path
            })
            .collect();

        session.replace_messages(vec![user_message("summary")]);
        session.save_to(dir).unwrap();

        let archives = archive_paths(dir, session.id);
        assert_eq!(archives.len(), 2);
        assert!(archives.contains(&fakes[2]));
        assert!(!fakes[0].exists());
        assert!(!fakes[1].exists());
    }

    #[test]
    fn delete_removes_archive_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("model", "/p");
        session.push_message(user_message("one"));
        session.push_message(user_message("two"));
        session.save_to(dir).unwrap();
        session.replace_messages(vec![user_message("summary")]);
        session.save_to(dir).unwrap();
        let archive_dir = archive_dir_for(dir, session.id);
        assert!(archive_dir.exists());

        TestSession::delete_from(session.id, dir).unwrap();
        assert!(!archive_dir.exists());
        assert!(!jsonl_path(dir, session.id).exists());
    }

    /// A rename with no new messages must survive restart, while a no-op
    /// append must not grow the file.
    #[test]
    fn append_writes_meta_only_when_it_changed() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("hi"));
        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        let path = jsonl_path(dir, session.id);
        let size_before = fs::metadata(&path).unwrap().len();
        log.append(&session).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), size_before);

        session.title = "renamed".into();
        session.updated_at = 42;
        log.append(&session).unwrap();

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(loaded.title, "renamed");
        assert_eq!(loaded.updated_at, 42);
    }

    #[test]
    fn migration_json_to_jsonl() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("legacy"));

        let json_path = json_path(dir, session.id);
        fs::write(&json_path, serde_json::to_vec(&session).unwrap()).unwrap();
        update_cwd_index(dir, &session.cwd, session.id).unwrap();

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(loaded.messages().len(), 1);

        let _log = SessionLog::rewrite(dir, &loaded).unwrap();

        assert!(!json_path.exists());
        assert!(jsonl_path(dir, session.id).exists());

        let reloaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(reloaded.messages().len(), 1);
        assert_eq!(reloaded.model, "m");
    }

    #[test]
    fn load_nonexistent_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let id = MakiId::generate();
        let err = TestSession::load_from(id, tmp.path()).unwrap_err();
        assert!(matches!(
            err,
            SessionError::Storage(StorageError::NotFound(_))
        ));
    }

    #[test_case("550e8400-e29b-41d4-a716-446655440000")]
    #[test_case("550e8400e29b41d4a716446655440000")]
    fn load_legacy_hex_filename_migrates_to_canonical(legacy: &str) {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let id: MakiId = legacy.parse().unwrap();
        let mut session: TestSession = Session::new("m", "/project");
        session.id = id;
        session.push_message(user_message("legacy"));
        let legacy_path = dir.join(format!("{legacy}.jsonl"));
        write_legacy_jsonl(&legacy_path, &session);

        let loaded = TestSession::load_from(id, dir).unwrap();
        assert_eq!(loaded.id, id);
        assert_eq!(loaded.messages().len(), 1);

        assert!(!legacy_path.exists());
        let canonical = jsonl_path(dir, id);
        assert!(canonical.exists());
    }

    #[test]
    fn list_filters_by_cwd() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut s1: TestSession = Session::new("m", "/project-a");
        let mut s2: TestSession = Session::new("m", "/project-b");
        let mut s3: TestSession = Session::new("m", "/project-a");
        s1.save_to(dir).unwrap();
        s2.save_to(dir).unwrap();
        s3.save_to(dir).unwrap();

        let list = TestSession::list_in("/project-a", dir).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|s| s.id != s2.id));
    }

    /// Rewrites the scan-cache title of `id` without touching the session
    /// file, so a later list showing [`TAMPERED_TITLE`] proves it was served
    /// from the cache instead of re-reading the file.
    fn tamper_cached_title(dir: &Path, id: MakiId) {
        let cache_path = dir.join(SCAN_CACHE_FILE);
        let mut cache: Value = serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
        let entry = cache
            .as_object_mut()
            .unwrap()
            .get_mut(&format!("{id}.jsonl"))
            .expect("session missing from scan cache");
        entry["header"]["title"] = TAMPERED_TITLE.into();
        fs::write(&cache_path, serde_json::to_vec(&cache).unwrap()).unwrap();
    }

    /// One scan must cache headers of every cwd, so reopening the picker
    /// here or in another project never re-reads unchanged files.
    #[test]
    fn list_serves_all_cwds_from_cache_after_one_scan() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut a: TestSession = Session::new("m", "/project-a");
        a.save_to(dir).unwrap();
        let mut b: TestSession = Session::new("m", "/project-b");
        b.save_to(dir).unwrap();
        TestSession::list_in("/project-a", dir).unwrap();

        tamper_cached_title(dir, a.id);
        tamper_cached_title(dir, b.id);
        let list_a = TestSession::list_in("/project-a", dir).unwrap();
        assert_eq!(list_a[0].title, TAMPERED_TITLE);
        let list_b = TestSession::list_in("/project-b", dir).unwrap();
        assert_eq!(list_b[0].title, TAMPERED_TITLE);
    }

    #[test]
    fn list_rescans_changed_file_and_prunes_deleted() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut s1: TestSession = Session::new("m", "/project");
        s1.push_message(user_message("hi"));
        let mut log = SessionLog::rewrite(dir, &s1).unwrap();
        let s2: TestSession = Session::new("m", "/project");
        SessionLog::rewrite(dir, &s2).unwrap();
        TestSession::list_in("/project", dir).unwrap();

        s1.title = "renamed".into();
        log.append(&s1).unwrap();
        TestSession::delete_from(s2.id, dir).unwrap();

        let list = TestSession::list_in("/project", dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "renamed");
        let cache: Value =
            serde_json::from_slice(&fs::read(dir.join(SCAN_CACHE_FILE)).unwrap()).unwrap();
        assert_eq!(cache.as_object().unwrap().len(), 1, "deleted entry pruned");
    }

    #[test]
    fn dirty_persisted_title_normalized_on_list_and_load() {
        const NORMALIZED: &str = "line one line two";
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut s: TestSession = Session::new("m", "/project");
        s.push_message(user_message("hi"));
        let mut log = SessionLog::rewrite(dir, &s).unwrap();
        s.title = "line one\n\n\tline two".into();
        log.append(&s).unwrap();

        let list = TestSession::list_in("/project", dir).unwrap();
        assert_eq!(list[0].title, NORMALIZED);
        assert_eq!(TestSession::load_from(s.id, dir).unwrap().title, NORMALIZED);
    }

    #[test_case(Some(b"{ not json".as_slice()) ; "corrupt_cache")]
    #[test_case(None ; "missing_cache")]
    fn list_survives_bad_scan_cache(content: Option<&[u8]>) {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut s: TestSession = Session::new("m", "/project");
        s.save_to(dir).unwrap();
        if let Some(content) = content {
            fs::write(dir.join(SCAN_CACHE_FILE), content).unwrap();
        }

        let list = TestSession::list_in("/project", dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, s.id);
    }

    fn save_with_time(session: &mut TestSession, dir: &Path, time: u64) {
        session.updated_at = time;
        SessionLog::rewrite(dir, session).unwrap();
        update_cwd_index(dir, &session.cwd, session.id).unwrap();
    }

    #[test]
    fn latest_returns_most_recent_for_cwd() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut s1: TestSession = Session::new("m", "/project");
        s1.title = "first".into();
        save_with_time(&mut s1, dir, 1000);

        let mut s2: TestSession = Session::new("m", "/other");
        save_with_time(&mut s2, dir, 2000);

        let mut s3: TestSession = Session::new("m", "/project");
        s3.title = "latest".into();
        save_with_time(&mut s3, dir, 3000);

        let latest = TestSession::latest_in("/project", dir).unwrap().unwrap();
        assert_eq!(latest.title, "latest");
    }

    #[test]
    fn latest_falls_back_when_index_stale() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.save_to(dir).unwrap();

        let index_path = dir.join(CWD_INDEX_FILE);
        let stale: HashMap<String, String> = [("/project".into(), "deleted-id".into())].into();
        fs::write(&index_path, serde_json::to_vec(&stale).unwrap()).unwrap();

        let latest = TestSession::latest_in("/project", dir).unwrap().unwrap();
        assert_eq!(latest.id, session.id);
    }

    #[test_case("short title", "short title" ; "short_passthrough")]
    #[test_case("", DEFAULT_TITLE ; "empty_defaults")]
    #[test_case(
        "This is a very long title that exceeds the sixty character limit and should be truncated at a word boundary",
        "This is a very long title that exceeds the sixty character…"
        ; "long_truncates_at_word"
    )]
    #[test_case("one\n\ntwo\t three", "one two three" ; "whitespace_collapses")]
    fn title_extraction(input: &str, expected: &str) {
        let messages: Vec<Value> = if input.is_empty() {
            vec![]
        } else {
            vec![user_message(input)]
        };
        assert_eq!(generate_title(&messages), expected);
    }

    #[test]
    fn delete_removes_file_and_cwd_index() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut s1: TestSession = Session::new("m", "/project");
        s1.save_to(dir).unwrap();
        let mut s2: TestSession = Session::new("m", "/other");
        s2.save_to(dir).unwrap();

        TestSession::delete_from(s1.id, dir).unwrap();
        assert!(!jsonl_path(dir, s1.id).exists());
        let index = load_cwd_index(dir);
        assert!(!index.values().any(|v| *v == s1.id.to_string()));
        assert_eq!(index.get("/other"), Some(&s2.id.to_string()));
    }

    #[test]
    fn delete_nonexistent_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let id = MakiId::generate();
        let err = TestSession::delete_from(id, tmp.path()).unwrap_err();
        assert!(matches!(
            err,
            SessionError::Storage(StorageError::NotFound(_))
        ));
    }

    #[test_case("550e8400-e29b-41d4-a716-446655440000")]
    #[test_case("550e8400e29b41d4a716446655440000")]
    fn delete_legacy_hex_filename_removes_file(legacy: &str) {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let id: MakiId = legacy.parse().unwrap();
        let mut session: TestSession = Session::new("m", "/project");
        session.id = id;
        session.push_message(user_message("legacy"));
        let legacy_path = dir.join(format!("{legacy}.jsonl"));
        write_legacy_jsonl(&legacy_path, &session);

        TestSession::delete_from(id, dir).unwrap();
        assert!(!legacy_path.exists());
        let canonical = jsonl_path(dir, id);
        assert!(!canonical.exists());
    }

    #[test]
    fn delete_removes_coexisting_json_and_jsonl() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("hi"));

        let jsonl_file = jsonl_path(dir, session.id);
        write_legacy_jsonl(&jsonl_file, &session);
        let json_file = json_path(dir, session.id);
        fs::write(&json_file, serde_json::to_vec(&session).unwrap()).unwrap();

        TestSession::delete_from(session.id, dir).unwrap();
        assert!(!jsonl_file.exists());
        assert!(!json_file.exists());
    }

    #[test]
    fn load_picks_jsonl_when_legacy_dual_file_exists() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let id: MakiId = LEGACY_HEX_ID.parse().unwrap();
        let mut jsonl_session: TestSession = Session::new("m", "/project");
        jsonl_session.id = id;
        jsonl_session.push_message(user_message("newer"));

        let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
        write_legacy_jsonl(&legacy_jsonl, &jsonl_session);

        let mut json_session: TestSession = Session::new("m", "/project");
        json_session.id = id;
        json_session.push_message(user_message("older"));
        let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
        fs::write(&legacy_json, serde_json::to_vec(&json_session).unwrap()).unwrap();

        let loaded = TestSession::load_from(id, dir).unwrap();
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.messages()[0], user_message("newer"));
    }

    #[test]
    fn load_dual_legacy_files_does_not_leave_duplicate_in_list() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let id: MakiId = LEGACY_HEX_ID.parse().unwrap();
        let mut jsonl_session: TestSession = Session::new("m", "/project");
        jsonl_session.id = id;
        jsonl_session.push_message(user_message("newer"));
        let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
        write_legacy_jsonl(&legacy_jsonl, &jsonl_session);

        let mut json_session: TestSession = Session::new("m", "/project");
        json_session.id = id;
        json_session.push_message(user_message("older"));
        let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
        fs::write(&legacy_json, serde_json::to_vec(&json_session).unwrap()).unwrap();

        TestSession::load_from(id, dir).unwrap();

        assert!(!legacy_json.exists(), "legacy .json sibling left behind");
        let list = TestSession::list_in("/project", dir).unwrap();
        assert_eq!(
            list.len(),
            1,
            "session shows up more than once in the picker"
        );
    }

    #[test]
    fn delete_drains_coexisting_legacy_json_and_jsonl() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let id: MakiId = LEGACY_HEX_ID.parse().unwrap();
        let mut session: TestSession = Session::new("m", "/project");
        session.id = id;
        session.push_message(user_message("legacy"));

        let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
        write_legacy_jsonl(&legacy_jsonl, &session);

        let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
        fs::write(&legacy_json, serde_json::to_vec(&session).unwrap()).unwrap();

        TestSession::delete_from(id, dir).unwrap();
        assert!(!legacy_jsonl.exists());
        assert!(!legacy_json.exists());
    }

    #[test]
    fn rewrite_removes_legacy_named_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let id: MakiId = LEGACY_HEX_ID.parse().unwrap();
        let mut session: TestSession = Session::new("m", "/project");
        session.id = id;
        session.push_message(user_message("legacy"));

        let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
        write_legacy_jsonl(&legacy_jsonl, &session);

        let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
        fs::write(&legacy_json, serde_json::to_vec(&session).unwrap()).unwrap();

        let _log = SessionLog::rewrite(dir, &session).unwrap();

        assert!(!legacy_jsonl.exists());
        assert!(!legacy_json.exists());
        assert!(jsonl_path(dir, id).exists());
    }

    #[test]
    fn load_migration_does_not_steal_latest_pointer() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let mut newest: TestSession = Session::new("m", "/project");
        newest.title = "newest".into();
        save_with_time(&mut newest, dir, 3000);

        let mut older: TestSession = Session::new("m", "/project");
        older.title = "older".into();
        older.updated_at = 1000;
        let json_path = json_path(dir, older.id);
        fs::write(&json_path, serde_json::to_vec(&older).unwrap()).unwrap();

        // Opening the older session migrates it to canonical jsonl, but must not
        // repoint cwd→latest at it.
        let loaded = TestSession::load_from(older.id, dir).unwrap();
        assert_eq!(loaded.title, "older");
        assert!(!json_path.exists());

        let latest = TestSession::latest_in("/project", dir).unwrap().unwrap();
        assert_eq!(latest.title, "newest");
    }

    #[test]
    fn load_surfaces_corrupt_header_id() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let id = MakiId::generate();
        let mut session: TestSession = Session::new("m", "/project");
        session.id = id;

        let path = jsonl_path(dir, id);
        write_legacy_jsonl(&path, &session);

        let corrupted =
            fs::read_to_string(&path)
                .unwrap()
                .replacen(&id.to_string(), "not-a-valid-id", 1);
        fs::write(&path, corrupted).unwrap();

        let err = TestSession::load_from(id, dir).unwrap_err();
        assert!(matches!(err, SessionError::CorruptHeaderId { .. }));
    }

    #[test]
    fn remove_from_cwd_index_matches_legacy_hex_value() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let legacy = "550e8400-e29b-41d4-a716-446655440000";
        let id: MakiId = legacy.parse().unwrap();
        let mut session: TestSession = Session::new("m", "/project");
        session.id = id;

        let mut index: HashMap<String, String> = HashMap::new();
        index.insert("/project".into(), legacy.to_string());
        fs::write(
            dir.join(CWD_INDEX_FILE),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();

        super::remove_from_cwd_index(dir, session.id).unwrap();
        let after = load_cwd_index(dir);
        assert!(!after.contains_key("/project"));
    }

    #[test]
    fn title_unicode_safe() {
        let input = "あ".repeat(100);
        let title = generate_title(&[user_message(&input)]);
        assert!(title.len() <= MAX_TITLE_LEN * 4);
        assert!(title.is_char_boundary(title.len()));
    }

    #[test]
    fn scan_headers_reads_both_formats() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let mut s1: TestSession = Session::new("m", "/project");
        s1.title = "jsonl-session".into();
        s1.save_to(dir).unwrap();

        let mut s2: TestSession = Session::new("m", "/project");
        s2.title = "json-session".into();
        let json_path = json_path(dir, s2.id);
        fs::write(&json_path, serde_json::to_vec(&s2).unwrap()).unwrap();

        let list = TestSession::list_in("/project", dir).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn load_wrong_version_legacy_returns_error() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("test/model", "/tmp");
        session.version = 999;
        let path = json_path(dir, session.id);
        fs::write(&path, serde_json::to_vec(&session).unwrap()).unwrap();

        let err = TestSession::load_from(session.id, dir).unwrap_err();
        assert!(matches!(
            err,
            SessionError::VersionMismatch { found: 999, .. }
        ));
    }

    /// A torn tail is what a crash mid-append leaves behind, and the file is
    /// the only thing standing between the user and their conversation, so the
    /// cursor `rewrite` hands back must describe the file it just wrote.
    #[test]
    fn rewrite_replaces_a_torn_file_and_keeps_appending() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("first"));
        drop(SessionLog::rewrite(dir, &session).unwrap());

        let path = jsonl_path(dir, session.id);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"t\":\"msg\",\"d\":{\"trun").unwrap();
        drop(file);

        session.push_message(assistant_message("reply"));
        let mut log = SessionLog::rewrite(dir, &session).unwrap();
        session.push_message(user_message("second"));
        log.append(&session).unwrap();
        drop(log);

        let reloaded = TestSession::load_from(session.id, dir).unwrap();
        assert_same_session(&reloaded, &session);
    }

    #[test]
    fn load_wrong_version_jsonl_returns_error() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let bad_header = serde_json::json!({
            "t": "header",
            "v": 999,
            "id": "01965087-4c71-7f00-8000-000000000000",
            "model": "m",
            "cwd": "/tmp",
            "created_at": 0
        });
        let id: MakiId = "01965087-4c71-7f00-8000-000000000000".parse().unwrap();
        let path = jsonl_path(dir, id);
        fs::write(&path, format!("{}\n", bad_header)).unwrap();

        let err = TestSession::load_from(id, dir).unwrap_err();
        assert!(matches!(
            err,
            SessionError::VersionMismatch { found: 999, .. }
        ));
    }

    #[test_case(StoredThinking::Off ; "off")]
    #[test_case(StoredThinking::Adaptive ; "adaptive")]
    #[test_case(StoredThinking::Effort { level: Effort::XHigh } ; "effort")]
    #[test_case(StoredThinking::Budget { tokens: 4096 } ; "budget")]
    fn stored_thinking_serde_round_trip(variant: StoredThinking) {
        let json = serde_json::to_string(&variant).unwrap();
        let parsed: StoredThinking = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, variant);
    }

    #[test_case("off", Ok(StoredThinking::Off) ; "off")]
    #[test_case("adaptive", Ok(StoredThinking::Adaptive) ; "adaptive")]
    #[test_case(" adaptive ", Ok(StoredThinking::Adaptive) ; "trims_whitespace")]
    #[test_case("4096", Ok(StoredThinking::Budget { tokens: 4096 }) ; "valid_budget")]
    #[test_case("1", Ok(StoredThinking::Budget { tokens: 1 }) ; "minimum_budget")]
    #[test_case("0", Err(ThinkingParseError::BudgetZero) ; "budget_zero")]
    #[test_case("fast", Err(ThinkingParseError::Unknown("fast".into())) ; "garbage")]
    #[test_case("high", Ok(StoredThinking::Effort { level: Effort::High }) ; "effort_level")]
    fn parse_setting(input: &str, expected: Result<StoredThinking, ThinkingParseError>) {
        assert_eq!(StoredThinking::parse_setting(input), expected);
    }

    // Six ascending values in a six-variant enum also proves ALL is complete.
    #[test]
    fn effort_all_ascending_with_increasing_percent() {
        for pair in Effort::ALL.windows(2) {
            assert!(pair[0] < pair[1], "ALL must be ascending");
            assert!(
                pair[0].percent() < pair[1].percent(),
                "percent must be strictly increasing"
            );
        }
    }

    #[test]
    fn effort_wire_strings_round_trip() {
        let expected = ["minimal", "low", "medium", "high", "xhigh", "max"];
        for (e, s) in Effort::ALL.into_iter().zip(expected) {
            assert_eq!(e.as_str(), s);
            assert_eq!(s.parse::<Effort>(), Ok(e));
        }
    }

    #[test_case(Effort::High, &[Effort::Low, Effort::Medium, Effort::High], Effort::High ; "exact_match")]
    #[test_case(Effort::Max, &[Effort::Low, Effort::Medium, Effort::High], Effort::High ; "downgrade_to_nearest_lower")]
    #[test_case(Effort::Minimal, &[Effort::Low, Effort::Medium], Effort::Low ; "below_lowest_takes_lowest")]
    #[test_case(Effort::Medium, &[], Effort::Medium ; "empty_supported_keeps_self")]
    #[test_case(Effort::Max, &[Effort::High, Effort::XHigh], Effort::XHigh ; "glm_max_snaps_to_xhigh")]
    fn effort_snap(level: Effort, supported: &[Effort], expected: Effort) {
        assert_eq!(level.snap(supported), expected);
    }

    #[test_case(Effort::Minimal, 32_768, 3_276 ; "minimal_ten_percent")]
    #[test_case(Effort::Medium, 32_768, 13_107 ; "medium_forty_percent")]
    #[test_case(Effort::Max, 32_768, 32_768 ; "max_full_budget")]
    #[test_case(Effort::Minimal, 4_096, 1_024 ; "small_max_floors_at_min")]
    #[test_case(Effort::Max, 512, 1_024 ; "tiny_max_raised_to_floor")]
    fn effort_budget(level: Effort, max: u32, expected: u32) {
        assert_eq!(level.budget(max), expected);
    }

    #[test_case(32_768, 32_768, Effort::Max ; "full_budget_is_max")]
    #[test_case(64_000, 32_768, Effort::Max ; "above_max_is_max")]
    #[test_case(0, 32_768, Effort::Minimal ; "zero_is_minimal")]
    #[test_case(13_107, 32_768, Effort::Medium ; "forty_percent_is_medium")]
    #[test_case(1_024, 0, Effort::Max ; "zero_max_saturates")]
    fn effort_from_budget(n: u32, max: u32, expected: Effort) {
        assert_eq!(Effort::from_budget(n, max), expected);
    }

    #[test]
    fn effort_budget_round_trips_at_realistic_max() {
        const MAX: u32 = 32_768;
        for e in Effort::ALL {
            assert_eq!(Effort::from_budget(e.budget(MAX), MAX), e);
        }
    }

    #[test]
    fn session_meta_backward_compat_defaults() {
        let json = r#"{"mode":"build"}"#;
        let meta: super::SessionMeta = serde_json::from_str(json).unwrap();
        assert!(meta.thinking.is_none());
        assert!(!meta.fast);
        assert!(!meta.workflow);
        assert!(meta.yolo.is_none());
    }

    #[test]
    fn session_meta_persists_through_save_load() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.meta.thinking = Some(StoredThinking::Budget { tokens: 8192 });
        session.meta.fast = true;
        session.meta.workflow = true;
        session.meta.yolo = Some(true);
        session.save_to(dir).unwrap();

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(
            loaded.meta.thinking,
            Some(StoredThinking::Budget { tokens: 8192 })
        );
        assert!(loaded.meta.fast);
        assert!(loaded.meta.workflow);
        assert_eq!(loaded.meta.yolo, Some(true));
    }

    #[test]
    fn crash_recovery_preserves_tool_outputs_around_corrupt_line() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("first"));
        session
            .tool_outputs
            .insert("t1".into(), Arc::new(serde_json::json!({"result": "ok"})));
        let mut log = SessionLog::rewrite(dir, &session).unwrap();
        log.append(&session).unwrap();

        let path = jsonl_path(dir, session.id);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"CORRUPT\n").unwrap();
        drop(file);
        append_raw_msg(&path, user_message("second"));

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(loaded.messages().len(), 2);
        assert!(loaded.tool_outputs().contains_key("t1"));
    }

    #[test]
    fn corrupt_header_line_only_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let id: MakiId = "01965087-4c71-7f00-8000-000000000000".parse().unwrap();
        let path = jsonl_path(dir, id);
        fs::write(&path, "NOT_A_HEADER\n").unwrap();

        let err = TestSession::load_from(id, dir).unwrap_err();
        assert!(matches!(
            err,
            SessionError::Storage(StorageError::NotFound(_))
        ));
    }

    #[test]
    fn empty_lines_in_jsonl_are_skipped() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("msg"));
        session.save_to(dir).unwrap();

        let path = jsonl_path(dir, session.id);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"\n\n\n").unwrap();
        drop(file);
        append_raw_msg(&path, user_message("after"));

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(loaded.messages().len(), 2);
    }

    #[test]
    fn unknown_record_type_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("first"));
        session.save_to(dir).unwrap();

        let path = jsonl_path(dir, session.id);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"t\":\"future_type\",\"d\":{}}\n")
            .unwrap();
        drop(file);
        append_raw_msg(&path, user_message("second"));

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_eq!(loaded.messages().len(), 2);
    }

    #[test]
    fn scan_returns_latest_title_after_multiple_appends() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("first"));
        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        session.title = "v1".into();
        session.push_message(assistant_message("reply"));
        log.append(&session).unwrap();

        session.title = "v2".into();
        session.push_message(user_message("second"));
        log.append(&session).unwrap();

        let list = TestSession::list_in("/project", dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "v2");
    }

    #[test]
    fn scan_returns_default_title_for_header_only_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session: TestSession = Session::new("m", "/project");
        let path = jsonl_path(dir, session.id);
        let header = serde_json::json!({"t":"header","v":2,"id":session.id,"model":"m","cwd":"/project","created_at":0});
        fs::write(&path, format!("{}\n", header)).unwrap();

        let list = TestSession::list_in("/project", dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, DEFAULT_TITLE);
    }

    #[test]
    fn scan_handles_large_meta_record() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("msg"));
        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        session.title = "big-meta".into();
        session.meta.input_draft = Some("x".repeat(TAIL_BUF as usize * 2));
        session.push_message(assistant_message("reply"));
        log.append(&session).unwrap();

        let list = TestSession::list_in("/project", dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "big-meta");
    }

    // -- The log never guesses --

    const PROPERTY_SEED: u64 = 0x2545_F491_4F6C_DD1D;
    const PROPERTY_STEPS: usize = 500;
    const MUTATION_KINDS: u64 = 8;
    const EXTERNAL_TRUNCATION: u64 = 12;

    /// Deterministic xorshift so a failure is always the same failure.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn tool_message(id: &str) -> Value {
        serde_json::json!({ "role": "assistant", "tool": id })
    }

    fn tool_ids(m: &Value) -> Vec<String> {
        m.get("tool")
            .and_then(Value::as_str)
            .map(|s| vec![s.to_owned()])
            .unwrap_or_default()
    }

    /// What the storage writer does: append while the epoch holds, rewrite the
    /// whole file otherwise.
    fn write_through(log: &mut SessionLog, dir: &Path, session: &TestSession) {
        match log.append(session) {
            Err(SessionError::LogDiverged { .. }) => {
                *log = SessionLog::rewrite(dir, session).unwrap()
            }
            other => other.unwrap(),
        }
    }

    #[track_caller]
    fn assert_same_session(loaded: &TestSession, expected: &TestSession) {
        assert_eq!(loaded.messages(), expected.messages(), "messages");
        assert_eq!(loaded.tool_outputs(), expected.tool_outputs(), "outputs");
        assert_eq!(
            loaded.subagent_messages(),
            expected.subagent_messages(),
            "subagent messages",
        );
        assert_eq!(loaded.title, expected.title, "title");
        assert_eq!(loaded.meta, expected.meta, "meta");
        assert_eq!(loaded.updated_at, expected.updated_at, "updated_at");
    }

    fn mutate(session: &mut TestSession, rng: &mut Rng, step: usize) {
        let slot = format!("t{}", rng.below(4));
        match rng.below(MUTATION_KINDS) {
            0 => session.push_message(user_message(&format!("msg-{step}"))),
            1 => {
                session.push_message(tool_message(&slot));
                session.push_message(assistant_message("reply"));
            }
            2 => session.insert_tool_output(slot, Arc::new(Value::from(format!("out-{step}")))),
            3 => {
                let len = rng.below(4) as usize;
                let msgs = (0..len)
                    .map(|i| user_message(&format!("sub-{i}")))
                    .collect();
                session.set_subagent_messages(slot, msgs);
            }
            4 => {
                let len = session.messages().len();
                session.truncate_messages(len.saturating_sub(1 + rng.below(3) as usize));
            }
            5 => session.replace_messages(vec![user_message(&format!("fresh-{step}"))]),
            6 => session.prune_orphans(tool_ids),
            _ => {
                session.set_title(format!("title-{step}"));
                session.set_meta(SessionMeta {
                    input_draft: Some(format!("draft-{step}")),
                    ..session.meta.clone()
                });
            }
        }
    }

    /// Every mutation kind in random order, snapshots dropped here and there
    /// like the writer coalescing them, and the file clobbered now and then.
    /// Whatever the script, reloading must give back the live session.
    #[test]
    fn random_mutation_script_round_trips_through_the_log() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        let mut log = SessionLog::rewrite(dir, &session).unwrap();
        let mut rng = Rng(PROPERTY_SEED);

        for step in 0..PROPERTY_STEPS {
            mutate(&mut session, &mut rng, step);
            // Dropping a snapshot is what coalescing does, and the next write
            // must still land on a file that matches.
            if rng.below(3) == 0 {
                continue;
            }
            if rng.below(EXTERNAL_TRUNCATION) == 0 {
                let path = jsonl_path(dir, session.id);
                let len = std::fs::metadata(&path).unwrap().len();
                OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(len / 2)
                    .unwrap();
            }
            write_through(&mut log, dir, &session);
        }
        write_through(&mut log, dir, &session);

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_same_session(&loaded, &session);
    }

    #[test]
    fn externally_truncated_log_is_rewritten_not_appended() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("hello"));
        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        let path = jsonl_path(dir, session.id);
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(0)
            .unwrap();

        session.push_message(assistant_message("reply"));
        assert!(matches!(
            log.append(&session),
            Err(SessionError::LogDiverged { .. }),
        ));
        drop(SessionLog::rewrite(dir, &session).unwrap());

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_same_session(&loaded, &session);
    }

    /// `Arc::make_mut` deep-copies the session while the writer holds the last
    /// snapshot, and a checkpoint that changes nothing must not pay for it.
    #[test]
    fn unchanged_checkpoint_does_not_clone_the_session() {
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("hello"));
        let snapshot = HistorySnapshot::new(session.messages().to_vec());
        let mut session = Arc::new(session);
        let meta = session.meta.clone();
        Session::checkpoint(&mut session, Some(&snapshot), meta.clone(), Value::Null);

        let held = Arc::clone(&session);
        Session::checkpoint(&mut session, Some(&snapshot), meta.clone(), Value::Null);
        assert!(Arc::ptr_eq(&held, &session), "no change, no clone");

        Session::checkpoint(
            &mut session,
            Some(&snapshot),
            SessionMeta {
                input_draft: Some("draft".into()),
                ..meta
            },
            Value::Null,
        );
        assert!(!Arc::ptr_eq(&held, &session));
        assert_eq!(session.meta.input_draft.as_deref(), Some("draft"));
        assert!(session.revision() > held.revision());
    }

    /// What the owner types sits in `meta`, so `content_revision` is what tells
    /// a keystroke, which can wait for the ones behind it, from a tool result,
    /// which has to be on disk before the next crash.
    #[test]
    fn a_meta_only_change_leaves_content_revision_alone() {
        let mut session: TestSession = Session::new("m", "/project");
        let (revision, content) = (session.revision(), session.content_revision());

        session.set_meta(SessionMeta {
            input_draft: Some(PENDING_DRAFT.into()),
            ..session.meta.clone()
        });
        assert!(session.revision() > revision, "still needs writing");
        assert_eq!(session.content_revision(), content, "but it can wait");

        session.push_message(user_message("hello"));
        assert!(session.content_revision() > content);
    }

    /// A mutator called with the value already there is not a change, and a
    /// truncate that cuts nothing must leave the epoch alone or every open
    /// cursor into the log dies for nothing.
    #[test]
    fn no_op_mutators_leave_the_session_and_its_cursors_alone() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.push_message(user_message("a"));
        session.push_message(assistant_message("b"));
        let mut log = SessionLog::rewrite(dir, &session).unwrap();
        let (revision, updated_at, epoch) = (session.revision(), session.updated_at, session.epoch);

        session.set_title(session.title.clone());
        session.set_meta(session.meta.clone());
        session.truncate_messages(session.messages().len());
        session.truncate_messages(session.messages().len() + 1);

        assert_eq!(session.revision(), revision);
        assert_eq!(session.updated_at, updated_at);
        assert_eq!(session.epoch, epoch);

        session.push_message(user_message("c"));
        log.append(&session).unwrap();
        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_same_session(&loaded, &session);
    }

    /// Every frame checkpoints, so a checkpoint that only grew the message list
    /// must stay a small append instead of rewriting the whole file.
    #[test]
    fn successive_checkpoints_from_one_run_stay_appendable() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut produced = HistorySnapshot::new(vec![user_message("a")]);
        let mut session: Arc<TestSession> = Arc::new(Session::new("m", "/project"));
        let meta = session.meta.clone();
        Session::checkpoint(&mut session, Some(&produced), meta.clone(), Value::Null);

        let mut log = SessionLog::rewrite(dir, &session).unwrap();
        for step in 0..3 {
            Arc::make_mut(&mut produced.messages).push(assistant_message(&format!("reply-{step}")));
            Session::checkpoint(&mut session, Some(&produced), meta.clone(), Value::Null);
            log.append(&session).unwrap();
        }

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_same_session(&loaded, &session);
    }

    /// The writer keeps the previous snapshot alive, so the UI ends up mutating
    /// a deep copy from `Arc::make_mut`. The copy inherits the run token, so the
    /// cursors the writer holds still describe it.
    #[test]
    fn append_cursor_survives_the_clone_arc_make_mut_hands_the_ui() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut base: TestSession = Session::new("m", "/project");
        base.push_message(user_message("a"));
        let mut log = SessionLog::rewrite(dir, &base).unwrap();

        let mut session = Arc::new(base);
        let held = Arc::clone(&session);
        let live = Arc::make_mut(&mut session);
        live.push_message(assistant_message("b"));
        live.insert_tool_output("t1".into(), Arc::new(Value::from("out")));
        live.set_subagent_messages("s1".into(), vec![user_message("sub")]);

        log.append(&session).unwrap();

        assert_eq!(held.messages().len(), 1, "the writer's snapshot is frozen");
        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_same_session(&loaded, &session);
    }

    /// The corruption the epoch exists for. A rewind mints a new run, so a
    /// snapshot still in flight carries the pre-rewind messages: longer than
    /// the rewritten log, yet sharing only its head. Going by length alone
    /// would splice its tail on and leave disk holding `[a, d, c]` while the
    /// session holds `[a, b, c]`.
    #[test]
    fn stale_snapshot_after_a_rewind_is_rewritten_not_spliced() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let produced = HistorySnapshot::new(vec![
            user_message("a"),
            assistant_message("b"),
            user_message("c"),
        ]);
        let mut session: Arc<TestSession> = Arc::new(Session::new("m", "/project"));
        let meta = session.meta.clone();
        Session::checkpoint(&mut session, Some(&produced), meta.clone(), Value::Null);
        let mut log = SessionLog::rewrite(dir, &session).unwrap();

        let live = Arc::make_mut(&mut session);
        live.truncate_messages(1);
        live.push_message(user_message("d"));
        write_through(&mut log, dir, &session);

        Session::checkpoint(&mut session, Some(&produced), meta, Value::Null);
        let path = jsonl_path(dir, session.id);
        let size_before = fs::metadata(&path).unwrap().len();
        assert!(matches!(
            log.append(&session),
            Err(SessionError::LogDiverged { .. }),
        ));
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            size_before,
            "a refused append must not have written half of itself"
        );
        drop(SessionLog::rewrite(dir, &session).unwrap());

        let loaded = TestSession::load_from(session.id, dir).unwrap();
        assert_same_session(&loaded, &session);
    }
}
