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
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use tracing::warn;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{StateDir, StorageError, atomic_write, now_epoch};

const SESSION_VERSION: u32 = 1;
const LOG_FORMAT_VERSION: u32 = 2;
pub const SESSIONS_DIR: &str = "sessions";
const CWD_INDEX_FILE: &str = "cwd_latest.json";
const DEFAULT_TITLE: &str = "New session";
const MAX_TITLE_LEN: usize = 60;

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
    file: File,
    saved_msg_count: usize,
    saved_tool_ids: HashSet<String>,
    saved_sub_msg_counts: HashMap<String, usize>,
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
        let path = dir.join(format!("{}.jsonl", session.id));
        let mut file = File::create(&path).map_err(StorageError::from)?;
        write_full_session(&mut file, session)?;
        file.sync_data().map_err(StorageError::from)?;

        update_cwd_index(dir, &session.cwd, &session.id)?;

        Ok(Self::cursor_from(session, file))
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
        let path = dir.join(format!("{session_id}.jsonl"));
        let session = load_jsonl::<M, U, T>(&path)?;

        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(StorageError::from)?;

        let log = Self::cursor_from(&session, file);
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

        self.file.write_all(&buf).map_err(StorageError::from)?;
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

        let path = dir.join(format!("{}.jsonl", session.id));
        let tmp = path.with_extension("jsonl.tmp");

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

        Ok(())
    }

    fn cursor_from<M, U, T>(session: &Session<M, U, T>, file: File) -> Self {
        Self {
            session_id: session.id.clone(),
            file,
            saved_msg_count: session.messages.len(),
            saved_tool_ids: session.tool_outputs.keys().cloned().collect(),
            saved_sub_msg_counts: sub_msg_snapshot(&session.subagent_messages),
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
            created_at: session.created_at,
        },
    )?;
    file.write_all(&buf).map_err(StorageError::from)?;
    for msg in &session.messages {
        buf.clear();
        append_record(&mut buf, &LogRecord::<&M, &U, &T>::Msg { d: msg })?;
        file.write_all(&buf).map_err(StorageError::from)?;
    }
    for (id, output) in &session.tool_outputs {
        buf.clear();
        append_record(
            &mut buf,
            &LogRecord::<&M, &U, &T>::Out {
                id: id.clone(),
                d: output,
            },
        )?;
        file.write_all(&buf).map_err(StorageError::from)?;
    }
    for (sub_id, msgs) in &session.subagent_messages {
        for msg in msgs {
            buf.clear();
            append_record(
                &mut buf,
                &LogRecord::<&M, &U, &T>::SubMsg {
                    sub: sub_id.clone(),
                    d: msg,
                },
            )?;
            file.write_all(&buf).map_err(StorageError::from)?;
        }
    }
    buf.clear();
    append_record(
        &mut buf,
        &LogRecord::<&M, &U, &T>::Meta {
            title: session.title.clone(),
            token_usage: &session.token_usage,
            updated_at: session.updated_at,
            meta: session.meta.clone(),
        },
    )?;
    file.write_all(&buf).map_err(StorageError::from)?;
    Ok(())
}

fn append_record<R: Serialize>(buf: &mut Vec<u8>, record: &R) -> Result<(), SessionError> {
    serde_json::to_writer(&mut *buf, record).map_err(StorageError::from)?;
    buf.push(b'\n');
    Ok(())
}

fn load_jsonl<M, U, T>(path: &Path) -> Result<Session<M, U, T>, SessionError>
where
    M: DeserializeOwned,
    U: DeserializeOwned + Default,
    T: DeserializeOwned,
{
    let file = File::open(path).map_err(StorageError::from)?;
    let reader = BufReader::new(file);
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

    for line_result in reader.lines() {
        let line = line_result.map_err(StorageError::from)?;
        line_count += 1;
        if line.is_empty() {
            continue;
        }
        let record: LogRecord<M, U, T> = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    path = %path.display(),
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
                id = h_id;
                model = h_model;
                cwd = h_cwd;
                created_at = h_created;
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
        return Err(StorageError::NotFound(path.display().to_string()).into());
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
        match ext {
            "jsonl" => {
                if let Some(summary) = scan_jsonl_header(cwd, &path) {
                    out.push(summary);
                }
            }
            "json" => {
                if let Some(summary) = scan_legacy_header(cwd, &path) {
                    out.push(summary);
                }
            }
            _ => {}
        }
    }
    Ok(out)
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
    if header.v != LOG_FORMAT_VERSION || header.cwd != cwd {
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
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem == CWD_INDEX_FILE.trim_end_matches(".json") {
            continue;
        }
        if path
            .extension()
            .is_some_and(|e| e == "json" || e == "jsonl")
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
        let jsonl_path = dir.join(format!("{id}.jsonl"));
        if jsonl_path.exists() {
            return load_jsonl(&jsonl_path);
        }

        let json_path = dir.join(format!("{id}.json"));
        if !json_path.exists() {
            return Err(StorageError::NotFound(id.into()).into());
        }
        let data = fs::read(&json_path).map_err(StorageError::from)?;
        let session: Self = serde_json::from_slice(&data).map_err(StorageError::from)?;
        if session.version != SESSION_VERSION {
            return Err(SessionError::VersionMismatch {
                found: session.version,
                expected: SESSION_VERSION,
            });
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
        let jsonl_gone = try_remove(&dir.join(format!("{id}.jsonl")))?;
        let json_gone = try_remove(&dir.join(format!("{id}.json")))?;

        if !jsonl_gone && !json_gone {
            return Err(StorageError::NotFound(id.into()).into());
        }

        remove_from_cwd_index(dir, id)?;
        Ok(())
    }

    pub fn migrate_to_jsonl(dir: &Path, session: &Self) -> Result<SessionLog, SessionError> {
        let log = SessionLog::create(dir, session)?;
        let _ = fs::remove_file(dir.join(format!("{}.json", session.id)));
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
    fn crash_recovery_truncated_line() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("survives"));
        session.save_to(dir).unwrap();

        let path = dir.join(format!("{}.jsonl", session.id));
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"t\":\"msg\",\"d\":{\"trun").unwrap();

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(loaded.messages.len(), 1);
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
    fn migration_json_to_jsonl() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("legacy"));

        let json_path = dir.join(format!("{}.json", session.id));
        fs::write(&json_path, serde_json::to_vec(&session).unwrap()).unwrap();
        update_cwd_index(dir, &session.cwd, &session.id).unwrap();

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(loaded.messages.len(), 1);

        let _log = TestSession::migrate_to_jsonl(dir, &loaded).unwrap();

        assert!(!json_path.exists());
        assert!(dir.join(format!("{}.jsonl", session.id)).exists());

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
        assert!(!dir.join(format!("{}.jsonl", s1.id)).exists());
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
    }

    #[test]
    fn session_meta_persists_through_save_load() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.meta.thinking = Some(StoredThinking::Budget { tokens: 8192 });
        session.meta.fast = true;
        session.save_to(dir).unwrap();

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(
            loaded.meta.thinking,
            Some(StoredThinking::Budget { tokens: 8192 })
        );
        assert!(loaded.meta.fast);
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
        let mut log = SessionLog::create(dir, &session).unwrap();
        log.append(&session).unwrap();

        let path = dir.join(format!("{}.jsonl", session.id));
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"CORRUPT\n").unwrap();
        file.write_all(
            serde_json::to_string(&serde_json::json!({"t":"msg","d": user_message("second")}))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        file.write_all(b"\n").unwrap();

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert!(loaded.tool_outputs.contains_key("t1"));
    }

    #[test]
    fn corrupt_header_line_only_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let id = "fake-session-id";
        let path = dir.join(format!("{id}.jsonl"));
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
        session.messages.push(user_message("msg"));
        session.save_to(dir).unwrap();

        let path = dir.join(format!("{}.jsonl", session.id));
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"\n\n\n").unwrap();
        file.write_all(
            serde_json::to_string(&serde_json::json!({"t":"msg","d": user_message("after")}))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        file.write_all(b"\n").unwrap();

        let loaded = TestSession::load_from(&session.id, dir).unwrap();
        assert_eq!(loaded.messages.len(), 2);
    }

    #[test]
    fn unknown_record_type_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("first"));
        session.save_to(dir).unwrap();

        let path = dir.join(format!("{}.jsonl", session.id));
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"t\":\"future_type\",\"d\":{}}\n")
            .unwrap();
        file.write_all(
            serde_json::to_string(&serde_json::json!({"t":"msg","d": user_message("second")}))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        file.write_all(b"\n").unwrap();

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
}
