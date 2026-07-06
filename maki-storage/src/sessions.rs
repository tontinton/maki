//! Session persistence with append-only, zstd-compressed JSONL logs.
//!
//! Each session is stored as a single `{uuid}.zst` file (one zstd frame per append, so a
//! truncated trailing frame only loses that turn). The header (frame 0) carries the
//! session title, so the `/sessions` listing decodes only that first line plus the file
//! mtime, never touching the rest. `SessionLog` tracks cursor state for O(delta)
//! incremental saves; a title change triggers a one-shot full rewrite.
//!
//! Legacy `.jsonl` and `.json` files are loaded read-only and migrated to the compressed
//! format when next opened for writing.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use tracing::warn;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use zstd::stream::{Decoder, Encoder};

use crate::{StateDir, StorageError, atomic_write, now_epoch};

const SESSION_VERSION: u32 = 1;
const LOG_FORMAT_VERSION: u32 = 3;
const LEGACY_JSONL_VERSION: u32 = 2;
const COMPRESS_LEVEL: i32 = 3;
pub const SESSIONS_DIR: &str = "sessions";
const CWD_INDEX_FILE: &str = "cwd_latest.json";
const DEFAULT_TITLE: &str = "New session";
const MAX_TITLE_LEN: usize = 60;
const ZST_EXT: &str = "zst";

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("incompatible session version {found} (expected {expected})")]
    VersionMismatch { found: u32, expected: u32 },
    #[error("session ID mismatch: log owns {log_id}, got {given_id}")]
    IdMismatch { log_id: String, given_id: String },
    #[error("cursor ahead of session (log has {saved}, session has {actual}); compact required")]
    CursorAhead { saved: usize, actual: usize },
}

/// Per-model token breakdown entry. Mirrors the four usage counters tracked by
/// the active provider; kept storage-local to avoid a circular dependency on
/// `maki-providers`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTokenUsage {
    #[serde(default)]
    pub input: u32,
    #[serde(default)]
    pub output: u32,
    #[serde(default)]
    pub cache_creation: u32,
    #[serde(default)]
    pub cache_read: u32,
}

impl StoredTokenUsage {
    pub fn total_input(&self) -> u32 {
        self.input + self.cache_read + self.cache_creation
    }

    pub fn total(&self) -> u32 {
        self.input + self.output + self.cache_creation + self.cache_read
    }
}

impl std::ops::AddAssign for StoredTokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input += rhs.input;
        self.output += rhs.output;
        self.cache_creation += rhs.cache_creation;
        self.cache_read += rhs.cache_read;
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<StoredSubagent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<StoredThinking>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub workflow: bool,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub usage_by_model: HashMap<String, StoredTokenUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session<M, U, T> {
    pub version: u32,
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub model: String,
    pub messages: Vec<M>,
    pub token_usage: U,
    #[serde(default = "HashMap::new")]
    pub tool_outputs: HashMap<String, T>,
    #[serde(default = "HashMap::new", skip_serializing_if = "HashMap::is_empty")]
    pub subagent_messages: HashMap<String, Vec<M>>,
    #[serde(flatten)]
    pub meta: SessionMeta,
    pub created_at: u64,
    pub updated_at: u64,
}

pub struct SessionSummary {
    pub id: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRule {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub effect: StoredEffect,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ThinkingParseError {
    #[error("unknown thinking value {0:?} (use off, adaptive, or a token budget)")]
    Unknown(String),
    #[error("thinking budget must be greater than zero")]
    BudgetZero,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum StoredThinking {
    Off,
    Adaptive,
    Budget { tokens: u32 },
}

impl StoredThinking {
    pub fn parse_setting(input: &str) -> Result<Self, ThinkingParseError> {
        match input.trim() {
            "off" => Ok(Self::Off),
            "adaptive" => Ok(Self::Adaptive),
            other => match other.parse::<u32>() {
                Ok(0) => Err(ThinkingParseError::BudgetZero),
                Ok(n) => Ok(Self::Budget { tokens: n }),
                Err(_) => Err(ThinkingParseError::Unknown(other.to_string())),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSubagent {
    pub tool_use_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Deserialize)]
struct LegacyHeader {
    version: u32,
    id: String,
    title: String,
    cwd: String,
    updated_at: u64,
}

pub trait TitleSource {
    fn first_user_text(&self) -> Option<&str>;
}

pub fn generate_title<M: TitleSource>(messages: &[M]) -> String {
    let first_user_text = messages.iter().find_map(|m| m.first_user_text());

    let Some(text) = first_user_text.map(str::trim).filter(|t| !t.is_empty()) else {
        return DEFAULT_TITLE.into();
    };

    if text.len() <= MAX_TITLE_LEN {
        return text.to_string();
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
        id: String,
        model: String,
        cwd: String,
        #[serde(default)]
        title: Option<String>,
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
        #[serde(flatten)]
        meta: SessionMeta,
    },
}

// -- SessionLog: append-only persistence --

pub struct SessionLog {
    session_id: String,
    dir: PathBuf,
    file: File,
    saved_msg_count: usize,
    saved_tool_ids: HashSet<String>,
    saved_sub_msg_counts: HashMap<String, usize>,
    saved_title: String,
}

fn sub_msg_snapshot<M>(map: &HashMap<String, Vec<M>>) -> HashMap<String, usize> {
    map.iter().map(|(k, v)| (k.clone(), v.len())).collect()
}

impl SessionLog {
    pub fn create<M, U, T>(dir: &Path, session: &Session<M, U, T>) -> Result<Self, SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        fs::create_dir_all(dir).map_err(StorageError::from)?;
        let path = write_current(dir, session)?;
        update_cwd_index(dir, &session.cwd, &session.id)?;
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(StorageError::from)?;
        Ok(Self::cursor_from(dir, session, file))
    }

    pub fn open<M, U, T>(
        dir: &Path,
        session_id: &str,
    ) -> Result<(Session<M, U, T>, Self), SessionError>
    where
        M: Serialize + DeserializeOwned,
        U: Serialize + DeserializeOwned + Default,
        T: Serialize + DeserializeOwned,
    {
        ensure_current_format::<M, U, T>(dir, session_id)?;
        let path = session_path(dir, session_id);
        let session = load_jsonl::<M, U, T>(&path)?;

        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(StorageError::from)?;

        let log = Self::cursor_from(dir, &session, file);
        Ok((session, log))
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn append<M, U, T>(&mut self, session: &Session<M, U, T>) -> Result<(), SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        if session.id != self.session_id {
            return Err(SessionError::IdMismatch {
                log_id: self.session_id.clone(),
                given_id: session.id.clone(),
            });
        }

        if session.title != self.saved_title {
            let dir = self.dir.clone();
            return self.compact(&dir, session);
        }

        if self.saved_msg_count > session.messages.len()
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
        {
            return Err(SessionError::CursorAhead {
                saved: self.saved_msg_count,
                actual: session.messages.len(),
            });
        }

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

        if buf.is_empty() {
            return Ok(());
        }

        append_record(
            &mut buf,
            &LogRecord::<&M, &U, &T>::Meta {
                title: session.title.clone(),
                token_usage: &session.token_usage,
                updated_at: session.updated_at,
                meta: session.meta.clone(),
            },
        )?;

        encode_frame(&mut self.file, &buf)?;
        self.file.sync_data().map_err(StorageError::from)?;

        self.saved_msg_count = new_msg_count;
        self.saved_tool_ids.extend(new_tool_ids);
        for (sub_id, count) in new_sub_counts {
            self.saved_sub_msg_counts.insert(sub_id, count);
        }

        Ok(())
    }

    pub fn compact<M, U, T>(
        &mut self,
        dir: &Path,
        session: &Session<M, U, T>,
    ) -> Result<(), SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        if session.id != self.session_id {
            return Err(SessionError::IdMismatch {
                log_id: self.session_id.clone(),
                given_id: session.id.clone(),
            });
        }

        let path = session_path(dir, &session.id);
        let tmp = path.with_extension("tmp");

        let mut tmp_file = File::create(&tmp).map_err(StorageError::from)?;
        write_full_session(&mut tmp_file, session)?;
        tmp_file.sync_data().map_err(StorageError::from)?;

        fs::rename(&tmp, &path).map_err(StorageError::from)?;

        self.file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(StorageError::from)?;
        self.saved_msg_count = session.messages.len();
        self.saved_tool_ids = session.tool_outputs.keys().cloned().collect();
        self.saved_sub_msg_counts = sub_msg_snapshot(&session.subagent_messages);
        self.saved_title = session.title.clone();

        Ok(())
    }

    fn cursor_from<M, U, T>(dir: &Path, session: &Session<M, U, T>, file: File) -> Self {
        Self {
            session_id: session.id.clone(),
            dir: dir.to_path_buf(),
            file,
            saved_msg_count: session.messages.len(),
            saved_tool_ids: session.tool_outputs.keys().cloned().collect(),
            saved_sub_msg_counts: sub_msg_snapshot(&session.subagent_messages),
            saved_title: session.title.clone(),
        }
    }
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
            id: session.id.clone(),
            model: session.model.clone(),
            cwd: session.cwd.clone(),
            title: Some(session.title.clone()),
            created_at: session.created_at,
        },
    )?;
    for msg in &session.messages {
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
        for msg in msgs {
            append_record(
                &mut buf,
                &LogRecord::<&M, &U, &T>::SubMsg {
                    sub: sub_id.clone(),
                    d: msg,
                },
            )?;
        }
    }
    append_record(
        &mut buf,
        &LogRecord::<&M, &U, &T>::Meta {
            title: session.title.clone(),
            token_usage: &session.token_usage,
            updated_at: session.updated_at,
            meta: session.meta.clone(),
        },
    )?;
    encode_frame(file, &buf)
}

fn append_record<R: Serialize>(buf: &mut Vec<u8>, record: &R) -> Result<(), SessionError> {
    serde_json::to_writer(&mut *buf, record).map_err(StorageError::from)?;
    buf.push(b'\n');
    Ok(())
}

fn parse_records<M, U, T>(
    bytes: &[u8],
    expected_version: u32,
) -> Result<Session<M, U, T>, SessionError>
where
    M: DeserializeOwned,
    U: DeserializeOwned + Default,
    T: DeserializeOwned,
{
    let mut line_count = 0usize;

    let mut id = String::new();
    let mut model = String::new();
    let mut cwd = String::new();
    let mut created_at = 0u64;
    let mut messages = Vec::new();
    let mut tool_outputs = HashMap::new();
    let mut subagent_messages: HashMap<String, Vec<M>> = HashMap::new();
    let mut title = DEFAULT_TITLE.to_string();
    let mut token_usage = U::default();
    let mut updated_at = 0u64;
    let mut meta = SessionMeta::default();
    let mut got_header = false;

    for line in bytes.split(|&b| b == b'\n') {
        line_count += 1;
        if line.is_empty() {
            continue;
        }
        let record: LogRecord<M, U, T> = match serde_json::from_slice(line) {
            Ok(r) => r,
            Err(e) => {
                warn!(
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
                title: h_title,
                created_at: h_created,
            } => {
                if v != expected_version {
                    return Err(SessionError::VersionMismatch {
                        found: v,
                        expected: expected_version,
                    });
                }
                id = h_id;
                model = h_model;
                cwd = h_cwd;
                created_at = h_created;
                if let Some(t) = h_title {
                    title = t;
                }
                got_header = true;
            }
            LogRecord::Msg { d } => messages.push(d),
            LogRecord::Out { id: out_id, d } => {
                tool_outputs.insert(out_id, d);
            }
            LogRecord::SubMsg { sub, d } => {
                subagent_messages.entry(sub).or_default().push(d);
            }
            LogRecord::Meta {
                title: m_title,
                token_usage: m_usage,
                updated_at: m_updated,
                meta: m_meta,
            } => {
                title = m_title;
                token_usage = m_usage;
                updated_at = m_updated;
                meta = m_meta;
            }
        }
    }

    if !got_header {
        return Err(StorageError::NotFound("session header".into()).into());
    }

    Ok(Session {
        version: SESSION_VERSION,
        id,
        title,
        cwd,
        model,
        messages,
        token_usage,
        tool_outputs,
        subagent_messages,
        meta,
        created_at,
        updated_at,
    })
}

fn encode_frame(file: &mut File, bytes: &[u8]) -> Result<(), SessionError> {
    let mut enc = Encoder::new(file, COMPRESS_LEVEL).map_err(StorageError::from)?;
    enc.write_all(bytes).map_err(StorageError::from)?;
    enc.finish().map_err(StorageError::from)?;
    Ok(())
}

fn decode_all(file: &File) -> Result<Vec<u8>, SessionError> {
    let mut dec = Decoder::new(file).map_err(StorageError::from)?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 65536];
    loop {
        match dec.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => {
                warn!(error = %e, "truncated frame, recovering complete frames");
                break;
            }
        }
    }
    Ok(out)
}

fn load_jsonl<M, U, T>(path: &Path) -> Result<Session<M, U, T>, SessionError>
where
    M: DeserializeOwned,
    U: DeserializeOwned + Default,
    T: DeserializeOwned,
{
    let file = File::open(path).map_err(StorageError::from)?;
    parse_records(&decode_all(&file)?, LOG_FORMAT_VERSION)
}

fn load_legacy_jsonl<M, U, T>(path: &Path) -> Result<Session<M, U, T>, SessionError>
where
    M: DeserializeOwned,
    U: DeserializeOwned + Default,
    T: DeserializeOwned,
{
    parse_records(
        &fs::read(path).map_err(StorageError::from)?,
        LEGACY_JSONL_VERSION,
    )
}

fn load_legacy_json<M, U, T>(path: &Path) -> Result<Session<M, U, T>, SessionError>
where
    M: DeserializeOwned,
    U: DeserializeOwned + Default,
    T: DeserializeOwned,
{
    let session: Session<M, U, T> =
        serde_json::from_slice(&fs::read(path).map_err(StorageError::from)?)
            .map_err(StorageError::from)?;
    if session.version != SESSION_VERSION {
        return Err(SessionError::VersionMismatch {
            found: session.version,
            expected: SESSION_VERSION,
        });
    }
    Ok(session)
}

fn session_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.{ZST_EXT}"))
}

fn write_current<M, U, T>(dir: &Path, session: &Session<M, U, T>) -> Result<PathBuf, SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    let path = session_path(dir, &session.id);
    let mut file = File::create(&path).map_err(StorageError::from)?;
    write_full_session(&mut file, session)?;
    file.sync_data().map_err(StorageError::from)?;
    Ok(path)
}

fn ensure_current_format<M, U, T>(dir: &Path, id: &str) -> Result<(), SessionError>
where
    M: Serialize + DeserializeOwned,
    U: Serialize + DeserializeOwned + Default,
    T: Serialize + DeserializeOwned,
{
    if session_path(dir, id).exists() {
        return Ok(());
    }
    let jsonl = dir.join(format!("{id}.jsonl"));
    if jsonl.exists() {
        let session = load_legacy_jsonl::<M, U, T>(&jsonl)?;
        write_current(dir, &session)?;
        let _ = fs::remove_file(&jsonl);
        return Ok(());
    }
    let json = dir.join(format!("{id}.json"));
    if json.exists() {
        let session = load_legacy_json::<M, U, T>(&json)?;
        write_current(dir, &session)?;
        let _ = fs::remove_file(&json);
        return Ok(());
    }
    Err(StorageError::NotFound(id.to_string()).into())
}

// -- CWD index --

fn load_cwd_index(dir: &Path) -> HashMap<String, String> {
    fs::read(dir.join(CWD_INDEX_FILE))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn update_cwd_index(dir: &Path, cwd: &str, session_id: &str) -> Result<(), StorageError> {
    let mut index = load_cwd_index(dir);
    index.insert(cwd.to_string(), session_id.to_string());
    atomic_write(&dir.join(CWD_INDEX_FILE), &serde_json::to_vec(&index)?)
}

fn try_remove(path: &Path) -> Result<bool, StorageError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn remove_from_cwd_index(dir: &Path, session_id: &str) -> Result<(), StorageError> {
    let mut index = load_cwd_index(dir);
    let before = index.len();
    index.retain(|_, v| v != session_id);
    if index.len() != before {
        atomic_write(&dir.join(CWD_INDEX_FILE), &serde_json::to_vec(&index)?)?;
    }
    Ok(())
}

// -- Header scanning for session list --

#[derive(Deserialize)]
struct JsonlHeader {
    v: u32,
    id: String,
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

fn scan_headers(cwd: &str, dir: &Path) -> Result<Vec<SessionSummary>, StorageError> {
    let mut out = Vec::new();
    for path in session_entries(dir)? {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let summary = match ext {
            ZST_EXT => scan_zst_header(cwd, &path),
            "jsonl" => scan_jsonl_header(cwd, &path),
            "json" => scan_legacy_header(cwd, &path),
            _ => None,
        };
        if let Some(summary) = summary {
            out.push(summary);
        }
    }
    Ok(out)
}

fn mtime_epoch(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

#[derive(Deserialize)]
struct ZstHeader {
    v: u32,
    id: String,
    cwd: String,
    #[serde(default)]
    title: Option<String>,
}

fn scan_zst_header(cwd: &str, path: &Path) -> Option<SessionSummary> {
    let file = File::open(path).ok()?;
    let dec = Decoder::new(file).ok()?;
    let mut reader = BufReader::new(dec);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let header: ZstHeader = serde_json::from_str(line.trim_end()).ok()?;
    if header.v != LOG_FORMAT_VERSION || header.cwd != cwd {
        return None;
    }
    Some(SessionSummary {
        id: header.id,
        title: header.title.unwrap_or_else(|| DEFAULT_TITLE.to_string()),
        updated_at: mtime_epoch(path).unwrap_or(0),
    })
}

const TAIL_BUF: u64 = 4096;

fn scan_jsonl_header(cwd: &str, path: &Path) -> Option<SessionSummary> {
    let mut file = File::open(path).ok()?;
    let header: JsonlHeader = {
        let mut reader = BufReader::new(&file);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        serde_json::from_str(line.trim_end()).ok()?
    };
    if header.v != LEGACY_JSONL_VERSION || header.cwd != cwd {
        return None;
    }

    let (title, updated_at) =
        read_last_meta(&mut file).unwrap_or_else(|| (DEFAULT_TITLE.to_string(), 0));

    Some(SessionSummary {
        id: header.id,
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

fn scan_legacy_header(cwd: &str, path: &Path) -> Option<SessionSummary> {
    let data = fs::read(path).ok()?;
    let h: LegacyHeader = serde_json::from_slice(&data).ok()?;
    if h.version != SESSION_VERSION || h.cwd != cwd {
        return None;
    }
    Some(SessionSummary {
        id: h.id,
        title: h.title,
        updated_at: h.updated_at,
    })
}

fn session_entries(dir: &Path) -> Result<Vec<PathBuf>, StorageError> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem == CWD_INDEX_FILE.trim_end_matches(".json") {
            continue;
        }
        if path
            .extension()
            .is_some_and(|e| e == "json" || e == "jsonl" || e == ZST_EXT)
        {
            entries.push(path);
        }
    }
    Ok(entries)
}

// -- Session impl --

impl<M, U, T> Session<M, U, T>
where
    M: Serialize + DeserializeOwned + TitleSource,
    U: Serialize + DeserializeOwned + Default,
    T: Serialize + DeserializeOwned,
{
    pub fn new(model: &str, cwd: &str) -> Self {
        let now = now_epoch();
        Self {
            version: SESSION_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            title: DEFAULT_TITLE.into(),
            cwd: cwd.into(),
            model: model.into(),
            messages: Vec::new(),
            token_usage: U::default(),
            tool_outputs: HashMap::new(),
            subagent_messages: HashMap::new(),
            meta: SessionMeta {
                mode: Some(StoredMode::Build),
                ..Default::default()
            },
            created_at: now,
            updated_at: now,
        }
    }

    pub fn save(&mut self, dir: &StateDir) -> Result<(), SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        self.save_to(&sessions_dir)
    }

    pub fn save_to(&mut self, dir: &Path) -> Result<(), SessionError> {
        self.updated_at = now_epoch();
        let _log = SessionLog::create(dir, self)?;
        Ok(())
    }

    pub fn load(id: &str, dir: &StateDir) -> Result<Self, SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        Self::load_from(id, &sessions_dir)
    }

    pub fn load_from(id: &str, dir: &Path) -> Result<Self, SessionError> {
        let zst_path = session_path(dir, id);
        if zst_path.exists() {
            return load_jsonl(&zst_path);
        }
        let jsonl_path = dir.join(format!("{id}.jsonl"));
        if jsonl_path.exists() {
            return load_legacy_jsonl(&jsonl_path);
        }
        let json_path = dir.join(format!("{id}.json"));
        if !json_path.exists() {
            return Err(StorageError::NotFound(id.into()).into());
        }
        load_legacy_json(&json_path)
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
        let index = load_cwd_index(dir);
        if let Some(id) = index.get(cwd)
            && let Ok(s) = Self::load_from(id, dir)
        {
            return Ok(Some(s));
        }
        let summaries = scan_headers(cwd, dir)?;
        let latest = summaries.into_iter().max_by_key(|s| s.updated_at);
        match latest {
            Some(s) => Self::load_from(&s.id, dir).map(Some),
            None => Ok(None),
        }
    }

    pub fn update_title_if_default(&mut self) {
        if self.title == DEFAULT_TITLE {
            self.title = generate_title(&self.messages);
        }
    }

    pub fn delete(id: &str, dir: &StateDir) -> Result<(), SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        Self::delete_from(id, &sessions_dir)
    }

    pub fn delete_from(id: &str, dir: &Path) -> Result<(), SessionError> {
        let zst_gone = try_remove(&session_path(dir, id))?;
        let jsonl_gone = try_remove(&dir.join(format!("{id}.jsonl")))?;
        let json_gone = try_remove(&dir.join(format!("{id}.json")))?;

        if !zst_gone && !jsonl_gone && !json_gone {
            return Err(StorageError::NotFound(id.into()).into());
        }

        remove_from_cwd_index(dir, id)?;
        Ok(())
    }

    pub fn migrate_to_compressed(dir: &Path, session: &Self) -> Result<SessionLog, SessionError> {
        let log = SessionLog::create(dir, session)?;
        let _ = fs::remove_file(dir.join(format!("{}.json", session.id)));
        let _ = fs::remove_file(dir.join(format!("{}.jsonl", session.id)));
        Ok(log)
    }
}

#[cfg(test)]
mod tests {
    use super::StoredThinking;
    use super::ThinkingParseError;
    use super::{
        CWD_INDEX_FILE, DEFAULT_TITLE, MAX_TITLE_LEN, SESSION_VERSION, TAIL_BUF, generate_title,
        load_cwd_index, update_cwd_index,
    };
    use super::{Session, SessionError, SessionLog, StorageError, TitleSource};
    use serde_json::Value;
    use std::collections::HashMap;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::Path;
    use tempfile::TempDir;
    use test_case::test_case;

    type TestSession = Session<Value, Value, Value>;

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
        serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": text}]
        })
    }

    fn assistant_message(text: &str) -> Value {
        serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": text}]
        })
    }

    fn append_raw_frame(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        let mut enc = zstd::stream::Encoder::new(&mut file, 3).unwrap();
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap();
    }

    #[test]
    fn roundtrip_save_load() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession =
            Session::new("anthropic/claude-sonnet-4", "/home/test/project");
        session.messages.push(user_message("hello"));
        session.subagent_messages.insert(
            "tool-1".into(),
            vec![user_message("sub-prompt"), assistant_message("sub-reply")],
        );
        session.save_to(dir).unwrap();

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.model, "anthropic/claude-sonnet-4");
        assert_eq!(loaded.cwd, "/home/test/project");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.version, SESSION_VERSION);
        assert_eq!(loaded.subagent_messages["tool-1"].len(), 2);
    }

    #[test]
    fn roundtrip_usage_by_model() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("anthropic/claude-sonnet-4", "/project");
        session.meta.usage_by_model.insert(
            "claude-sonnet-4".into(),
            super::StoredTokenUsage {
                input: 100,
                output: 20,
                cache_creation: 5,
                cache_read: 40,
            },
        );
        session.meta.usage_by_model.insert(
            "claude-haiku-4".into(),
            super::StoredTokenUsage {
                input: 30,
                output: 10,
                ..Default::default()
            },
        );
        session.save_to(dir).unwrap();

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        let sonnet = &loaded.meta.usage_by_model["claude-sonnet-4"];
        assert_eq!(sonnet.input, 100);
        assert_eq!(sonnet.output, 20);
        assert_eq!(sonnet.cache_read, 40);
        assert_eq!(sonnet.total_input(), 145);
        assert_eq!(loaded.meta.usage_by_model["claude-haiku-4"].total(), 40);
    }

    #[test]
    fn usage_by_model_absent_on_legacy_session() {
        let json = r#"{"t":"header","v":2,"id":"x","model":"m","cwd":"/","created_at":0}
{"t":"meta","title":"t","token_usage":null,"updated_at":0}"#;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("x.jsonl");
        fs::write(&path, json).unwrap();
        let loaded = TestSession::load_from("x", tmp.path()).unwrap();
        assert!(loaded.meta.usage_by_model.is_empty());
    }

    #[test]
    fn roundtrip_jsonl_incremental() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("first"));

        let mut log = SessionLog::create(dir, &session).unwrap();

        session.messages.push(assistant_message("reply"));
        session.messages.push(user_message("second"));
        session
            .tool_outputs
            .insert("tool-1".into(), serde_json::json!({"result": "ok"}));
        session
            .subagent_messages
            .insert("sub-1".into(), vec![user_message("sub-prompt")]);
        log.append(&session).unwrap();

        session
            .subagent_messages
            .get_mut("sub-1")
            .unwrap()
            .push(assistant_message("sub-reply"));
        session
            .subagent_messages
            .insert("sub-2".into(), vec![user_message("sub-2-prompt")]);
        log.append(&session).unwrap();

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.tool_outputs.len(), 1);
        assert!(loaded.tool_outputs.contains_key("tool-1"));
        assert_eq!(loaded.subagent_messages["sub-1"].len(), 2);
        assert_eq!(loaded.subagent_messages["sub-2"].len(), 1);
    }

    #[test]
    fn append_wrong_session_returns_id_mismatch() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session_a: TestSession = Session::new("m", "/project");
        let session_b: TestSession = Session::new("m", "/project");
        let mut log = SessionLog::create(dir, &session_a).unwrap();

        let err = log.append(&session_b).unwrap_err();
        assert!(matches!(err, SessionError::IdMismatch { .. }));
    }

    #[test]
    fn crash_recovery_truncated_frame() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("survives"));
        let path = dir.join(format!("{}.zst", session.id));

        let first_frame_size = {
            let _log = SessionLog::create(dir, &session).unwrap();
            fs::metadata(&path).unwrap().len()
        };

        let (_loaded, mut log) = SessionLog::open::<Value, Value, Value>(dir, &session.id).unwrap();
        session.messages.push(user_message("crashed"));
        log.append(&session).unwrap();
        drop(log);

        // Crash mid-append: keep only the first, complete frame.
        let data = fs::read(&path).unwrap();
        fs::write(&path, &data[..first_frame_size as usize]).unwrap();

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0], user_message("survives"));
    }

    #[test]
    fn rewind_compact() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        for i in 0..10 {
            session.messages.push(user_message(&format!("msg-{i}")));
        }
        session.subagent_messages.insert(
            "sub-1".into(),
            vec![user_message("sub-prompt"), assistant_message("sub-reply")],
        );
        let mut log = SessionLog::create(dir, &session).unwrap();

        session.messages.truncate(5);
        session.tool_outputs.clear();
        session.subagent_messages.remove("sub-1");
        log.compact(dir, &session).unwrap();

        session.messages.push(user_message("after-compact-1"));
        session.messages.push(user_message("after-compact-2"));
        session.messages.push(user_message("after-compact-3"));
        log.append(&session).unwrap();

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(loaded.messages.len(), 8);
        assert!(loaded.subagent_messages.is_empty());
    }

    #[test]
    fn migration_legacy_to_compressed() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("legacy"));

        let json_path = dir.join(format!("{}.json", session.id));
        fs::write(&json_path, serde_json::to_vec(&session).unwrap()).unwrap();
        update_cwd_index(dir, &session.cwd, &session.id).unwrap();

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(loaded.messages.len(), 1);

        let _log = TestSession::migrate_to_compressed(dir, &loaded).unwrap();

        assert!(!json_path.exists());
        assert!(dir.join(format!("{}.zst", session.id)).exists());
        assert!(!dir.join(format!("{}.meta", session.id)).exists());

        let reloaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(reloaded.messages.len(), 1);
        assert_eq!(reloaded.model, "m");
    }

    #[test]
    fn load_nonexistent_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let err = TestSession::load_from("nonexistent-id", tmp.path()).unwrap_err();
        assert!(matches!(
            err,
            SessionError::Storage(StorageError::NotFound(_))
        ));
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

    fn save_with_time(session: &mut TestSession, dir: &Path, time: u64) {
        session.updated_at = time;
        SessionLog::create(dir, session).unwrap();
        update_cwd_index(dir, &session.cwd, &session.id).unwrap();
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

        TestSession::delete_from(&s1.id, dir).unwrap();
        assert!(!dir.join(format!("{}.zst", s1.id)).exists());
        let index = load_cwd_index(dir);
        assert!(!index.values().any(|v| v == &s1.id));
        assert_eq!(index.get("/other"), Some(&s2.id));
    }

    #[test]
    fn delete_nonexistent_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let err = TestSession::delete_from("nonexistent", tmp.path()).unwrap_err();
        assert!(matches!(
            err,
            SessionError::Storage(StorageError::NotFound(_))
        ));
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
        let json_path = dir.join(format!("{}.json", s2.id));
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
        let path = dir.join(format!("{}.json", session.id));
        fs::write(&path, serde_json::to_vec(&session).unwrap()).unwrap();

        let err = TestSession::load_from(&session.id, dir).unwrap_err();
        assert!(matches!(
            err,
            SessionError::VersionMismatch { found: 999, .. }
        ));
    }

    #[test]
    fn open_roundtrip_resumes_append() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("first"));

        let mut log = SessionLog::create(dir, &session).unwrap();
        session.messages.push(assistant_message("reply"));
        log.append(&session).unwrap();
        drop(log);

        let (loaded, mut log) = SessionLog::open::<Value, Value, Value>(dir, &session.id).unwrap();
        assert_eq!(loaded.messages.len(), 2);

        session.messages.push(user_message("second"));
        log.append(&session).unwrap();
        drop(log);

        let reloaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(reloaded.messages.len(), 3);
    }

    #[test]
    fn load_wrong_version_jsonl_returns_error() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let bad_header = serde_json::json!({
            "t": "header",
            "v": 999,
            "id": "test-id",
            "model": "m",
            "cwd": "/tmp",
            "created_at": 0
        });
        let path = dir.join("test-id.jsonl");
        fs::write(&path, format!("{}\n", bad_header)).unwrap();

        let err = TestSession::load_from("test-id", dir).unwrap_err();
        assert!(matches!(
            err,
            SessionError::VersionMismatch { found: 999, .. }
        ));
    }

    #[test_case(StoredThinking::Off ; "off")]
    #[test_case(StoredThinking::Adaptive ; "adaptive")]
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
    fn parse_setting(input: &str, expected: Result<StoredThinking, ThinkingParseError>) {
        assert_eq!(StoredThinking::parse_setting(input), expected);
    }

    #[test]
    fn session_meta_backward_compat_defaults() {
        let json = r#"{"mode":"build"}"#;
        let meta: super::SessionMeta = serde_json::from_str(json).unwrap();
        assert!(meta.thinking.is_none());
        assert!(!meta.fast);
        assert!(!meta.workflow);
    }

    #[test]
    fn session_meta_persists_through_save_load() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.meta.thinking = Some(StoredThinking::Budget { tokens: 8192 });
        session.meta.fast = true;
        session.meta.workflow = true;
        session.save_to(dir).unwrap();

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(
            loaded.meta.thinking,
            Some(StoredThinking::Budget { tokens: 8192 })
        );
        assert!(loaded.meta.fast);
        assert!(loaded.meta.workflow);
    }

    #[test]
    fn crash_recovery_preserves_tool_outputs_around_corrupt_line() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("first"));
        session
            .tool_outputs
            .insert("t1".into(), serde_json::json!({"result": "ok"}));
        let log = SessionLog::create(dir, &session).unwrap();
        drop(log);

        let path = dir.join(format!("{}.zst", session.id));
        let second =
            serde_json::to_string(&serde_json::json!({"t":"msg","d": user_message("second")}))
                .unwrap();
        append_raw_frame(&path, format!("CORRUPT\n{second}\n").as_bytes());

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert!(loaded.tool_outputs.contains_key("t1"));
    }

    #[test]
    fn corrupt_header_line_only_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let id = "fake-session-id";
        let path = dir.join(format!("{id}.zst"));
        append_raw_frame(&path, b"NOT_A_HEADER\n");

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
        session.messages.push(user_message("msg"));
        let log = SessionLog::create(dir, &session).unwrap();
        drop(log);

        let path = dir.join(format!("{}.zst", session.id));
        let after =
            serde_json::to_string(&serde_json::json!({"t":"msg","d": user_message("after")}))
                .unwrap();
        append_raw_frame(&path, format!("\n\n\n{after}\n").as_bytes());

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(loaded.messages.len(), 2);
    }

    #[test]
    fn unknown_record_type_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("first"));
        let log = SessionLog::create(dir, &session).unwrap();
        drop(log);

        let path = dir.join(format!("{}.zst", session.id));
        let second =
            serde_json::to_string(&serde_json::json!({"t":"msg","d": user_message("second")}))
                .unwrap();
        append_raw_frame(
            &path,
            format!("{{\"t\":\"future_type\",\"d\":{{}}}}\n{second}\n").as_bytes(),
        );

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(loaded.messages.len(), 2);
    }

    #[test]
    fn scan_returns_latest_title_after_multiple_appends() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("first"));
        let mut log = SessionLog::create(dir, &session).unwrap();

        session.title = "v1".into();
        session.messages.push(assistant_message("reply"));
        log.append(&session).unwrap();

        session.title = "v2".into();
        session.messages.push(user_message("second"));
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
        let path = dir.join(format!("{}.jsonl", session.id));
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
        session.messages.push(user_message("msg"));
        let mut log = SessionLog::create(dir, &session).unwrap();

        session.title = "big-meta".into();
        session.meta.input_draft = Some("x".repeat(TAIL_BUF as usize * 2));
        session.messages.push(assistant_message("reply"));
        log.append(&session).unwrap();

        let list = TestSession::list_in("/project", dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "big-meta");
    }

    #[test]
    fn title_rename_round_trips_without_sidecar() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("first"));
        let mut log = SessionLog::create(dir, &session).unwrap();
        assert!(!dir.join(format!("{}.meta", session.id)).exists());

        session.title = "renamed".into();
        session.messages.push(assistant_message("reply"));
        log.append(&session).unwrap();
        assert!(!dir.join(format!("{}.meta", session.id)).exists());

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(loaded.title, "renamed");
        assert_eq!(loaded.messages.len(), 2);

        let list = TestSession::list_in("/project", dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "renamed");
    }

    #[test]
    fn load_wrong_version_zst_returns_error() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let path = dir.join("test-id.zst");
        let header = serde_json::json!({"t":"header","v":999,"id":"test-id","model":"m","cwd":"/tmp","created_at":0});
        append_raw_frame(&path, format!("{}\n", header).as_bytes());

        let err = TestSession::load_from("test-id", dir).unwrap_err();
        assert!(matches!(
            err,
            SessionError::VersionMismatch { found: 999, .. }
        ));
    }
}
