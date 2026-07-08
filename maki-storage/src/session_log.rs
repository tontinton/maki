//! Per-session folder reader/writer: `SessionReader`/`SessionWriter` capability
//! split (§A.0(5), §13), and the linear C1 load path.
//!
//! Acquiring the `fs4` exclusive lock on the sentinel `sessions/<id>/lock` is
//! the ONLY way to construct `SessionWriter`; lock contention (or fsync
//! failure, or unsupported version) yields a `SessionReader`, which has NO
//! append methods. Read-only mode is the absence of capability, not a flag.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;
use serde_json::value::RawValue;
use tracing::warn;

use crate::paths::{lock_path, log_path, meta_path, renders_path, session_dir};
use crate::renders::{self, RenderStore};
use crate::tree::{Header, MessageId, MessageNode, NodeRef, Role, ToolUseId, TreeRecord};
use crate::{StorageError, atomic_write, fsync_dir, now_epoch};

const LOG_VERSION: u32 = 3;

/// C1 linear model: a chain of message nodes, sub_msgs (raw), meta, warnings.
pub struct LoadedSession {
    pub header: Header,
    pub messages: Vec<MessageNode>,
    pub sub_msgs: Vec<crate::tree::SubMsgRecord>,
    pub meta: crate::tree::MetaRecord,
    pub warnings: Vec<String>,
}

/// Read-only handle. Constructed when the lock cannot be acquired (second
/// instance), or after a fsync-failure downgrade (§13). Has NO append methods.
pub struct SessionReader {
    session_dir: PathBuf,
    pub loaded: LoadedSession,
    renders: Option<RenderStore>,
}

impl SessionReader {
    pub fn session_id(&self) -> &str {
        &self.loaded.header.session_id
    }

    pub fn renders(&mut self) -> Result<&mut RenderStore, StorageError> {
        if self.renders.is_none() {
            let store =
                RenderStore::open(&renders_path(&self.session_dir)).map_err(StorageError::from)?;
            self.renders = Some(store);
        }
        Ok(self.renders.as_mut().unwrap())
    }
}

/// Write-capable handle. The exclusive lock on `sessions/<id>/lock` is held for
/// its lifetime; dropping releases it (§13).
pub struct SessionWriter {
    session_dir: PathBuf,
    pub loaded: LoadedSession,
    log_file: File,
    lock: File,
    renders: RenderStore,
    compressor: zstd::bulk::Compressor<'static>,
    unclean: bool,
}

impl SessionWriter {
    pub fn session_id(&self) -> &str {
        &self.loaded.header.session_id
    }

    pub fn renders(&mut self) -> &mut RenderStore {
        &mut self.renders
    }

    pub fn append_message(&mut self, node: MessageNode) -> Result<MessageId, StorageError> {
        if self.unclean {
            return Err(unclean_error());
        }
        let mut buf = serde_json::to_vec(&TreeRecord::Message(node.clone()))?;
        buf.push(b'\n');
        self.log_file.write_all(&buf)?;
        let id = node.id.clone();
        self.loaded.messages.push(node);
        Ok(id)
    }

    pub fn append_sub_msg(
        &mut self,
        record: crate::tree::SubMsgRecord,
    ) -> Result<(), StorageError> {
        if self.unclean {
            return Err(unclean_error());
        }
        let mut buf = serde_json::to_vec(&TreeRecord::SubMsg(record.clone()))?;
        buf.push(b'\n');
        self.log_file.write_all(&buf)?;
        self.loaded.sub_msgs.push(record);
        Ok(())
    }

    pub fn append_render(&mut self, id: &ToolUseId, frame: &[u8]) -> Result<(), StorageError> {
        if self.unclean {
            return Err(unclean_error());
        }
        self.renders
            .append(id, frame, &mut self.compressor)
            .map_err(StorageError::from)
    }

    pub fn write_meta(&mut self) -> Result<(), StorageError> {
        if self.unclean {
            return Err(unclean_error());
        }
        let mut meta = self.loaded.meta.clone();
        meta.updated_at = crate::now_epoch();
        let json = serde_json::to_vec_pretty(&meta)?;
        atomic_write(&meta_path(&self.session_dir), &json)
    }

    pub fn sync(&mut self) -> Result<(), StorageError> {
        if self.unclean {
            return Err(unclean_error());
        }
        if let Err(e) = self.log_file.sync_data() {
            self.downgrade(e);
            return Err(unclean_error());
        }
        Ok(())
    }

    fn downgrade(&mut self, err: std::io::Error) {
        warn!(error = %err, "fsync failed; downgrading writer to read-only");
        self.unclean = true;
        let _ = FileExt::unlock(&self.lock);
    }

    /// Rewrite the folder to the given linear message set: write-new-log-then-
    /// atomic-swap, also rewriting `renders.bin` without pruned tool_use_ids.
    /// TEMPORARY: replaced by leaf-move rewind in PR-3 (§4).
    pub fn rewrite_linear(
        &mut self,
        messages: Vec<MessageNode>,
        keep_render_ids: &HashSet<ToolUseId>,
    ) -> Result<(), StorageError> {
        if self.unclean {
            return Err(unclean_error());
        }
        let tmp_dir = self.session_dir.with_extension("rewrite.tmp");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir)?;

        let tmp_log = log_path(&tmp_dir);
        let tmp_renders = renders_path(&tmp_dir);
        let tmp_meta = meta_path(&tmp_dir);

        let mut log = File::create(&tmp_log)?;
        serde_json::to_writer(&mut log, &TreeRecord::Header(self.loaded.header.clone()))?;
        log.write_all(b"\n")?;
        for m in &messages {
            serde_json::to_writer(&mut log, &TreeRecord::Message(m.clone()))?;
            log.write_all(b"\n")?;
        }
        log.sync_all()?;

        let mut new_renders = RenderStore::open(&tmp_renders)?;
        let mut comp = renders::new_compressor()?;
        for id in keep_render_ids {
            if let Some(frame) = self.renders.get(id) {
                new_renders.append(id, &frame, &mut comp)?;
            }
        }
        drop((new_renders, comp));

        let meta_json = serde_json::to_vec_pretty(&self.loaded.meta)?;
        let mut mf = File::create(&tmp_meta)?;
        mf.write_all(&meta_json)?;
        mf.sync_all()?;
        fsync_dir(&tmp_dir)?;

        let final_log = log_path(&self.session_dir);
        let final_renders = renders_path(&self.session_dir);
        let final_meta = meta_path(&self.session_dir);
        fs::rename(&tmp_log, &final_log)?;
        fs::rename(&tmp_renders, &final_renders)?;
        fs::rename(&tmp_meta, &final_meta)?;
        let _ = fs::remove_dir_all(&tmp_dir);
        fsync_dir(&self.session_dir)?;

        self.log_file = OpenOptions::new().append(true).open(&final_log)?;
        self.renders = RenderStore::open(&final_renders)?;
        self.compressor = renders::new_compressor()?;
        self.loaded.messages = messages;
        Ok(())
    }
}

impl Drop for SessionWriter {
    fn drop(&mut self) {
        if !self.unclean {
            let _ = self.log_file.sync_data();
        }
        let _ = FileExt::unlock(&self.lock);
    }
}

fn unclean_error() -> StorageError {
    StorageError::Io(std::io::Error::other(
        "session writer downgraded after fsync failure",
    ))
}

pub enum OpenResult {
    Writer(SessionWriter),
    Reader(SessionReader),
    Unsupported(u32),
    Error(StorageError),
}

/// Open a session folder. Tries to acquire the writer lock; on contention (or
/// fsync failure) returns a `SessionReader`.
pub fn open(base: &Path, id: &str) -> OpenResult {
    let dir = session_dir(base, id);
    let loaded = match load_folder(&dir, id) {
        Ok(l) => l,
        Err(StorageError::NotFound(_)) => LoadedSession {
            header: init_header(id, "", now_epoch()),
            messages: Vec::new(),
            sub_msgs: Vec::new(),
            meta: crate::tree::MetaRecord {
                title: String::new(),
                cwd: String::new(),
                model: String::new(),
                updated_at: now_epoch(),
                migration: None,
                meta: crate::sessions::SessionMeta::default(),
            },
            warnings: Vec::new(),
        },
        Err(e) => return OpenResult::Error(e),
    };
    if loaded.header.version > LOG_VERSION {
        return OpenResult::Unsupported(loaded.header.version);
    }
    if fs::create_dir_all(&dir).is_err() {
        return OpenResult::Error(StorageError::NotFound(id.into()));
    }
    let lock = match File::create(lock_path(&dir)) {
        Ok(f) => f,
        Err(e) => return OpenResult::Error(StorageError::Io(e)),
    };
    if !FileExt::try_lock_exclusive(&lock).unwrap_or(false) {
        let reader = SessionReader {
            session_dir: dir,
            loaded,
            renders: None,
        };
        return OpenResult::Reader(reader);
    }
    let log_file = match OpenOptions::new()
        .append(true)
        .create(true)
        .open(log_path(&dir))
    {
        Ok(f) => f,
        Err(e) => {
            let _ = FileExt::unlock(&lock);
            return OpenResult::Error(StorageError::Io(e));
        }
    };
    let renders = match RenderStore::open(&renders_path(&dir)) {
        Ok(r) => r,
        Err(e) => {
            let _ = FileExt::unlock(&lock);
            return OpenResult::Error(StorageError::Io(e));
        }
    };
    let compressor = match renders::new_compressor() {
        Ok(c) => c,
        Err(e) => {
            let _ = FileExt::unlock(&lock);
            return OpenResult::Error(StorageError::Io(e));
        }
    };
    let writer = SessionWriter {
        session_dir: dir,
        loaded,
        log_file,
        lock,
        renders,
        compressor,
        unclean: false,
    };
    OpenResult::Writer(writer)
}

pub fn load_folder(dir: &Path, id: &str) -> Result<LoadedSession, StorageError> {
    let log = log_path(dir);
    let mut warnings = Vec::new();
    let mut header: Option<Header> = None;
    let mut messages = Vec::new();
    let mut sub_msgs = Vec::new();

    let file = File::open(&log).map_err(|e| StorageError::NotFound(format!("{id}: {e}")))?;
    let reader = BufReader::new(file);
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        match crate::tree::parse_line(&line) {
            Ok(Some(TreeRecord::Header(h))) => header = Some(h),
            Ok(Some(TreeRecord::Message(m))) => messages.push(m),
            Ok(Some(TreeRecord::SubMsg(s))) => sub_msgs.push(s),
            Ok(None) => {}
            Err(e) => {
                let msg = format!("log.jsonl:{}: {e}", i + 1);
                warn!(error = %e, line = i + 1, "skipping malformed log record");
                warnings.push(msg);
            }
        }
    }
    let header = header.ok_or_else(|| StorageError::NotFound(id.into()))?;
    let meta = load_meta(dir).unwrap_or_else(|_| crate::tree::MetaRecord {
        title: String::new(),
        cwd: header.cwd.clone(),
        model: String::new(),
        updated_at: header.created_at,
        migration: None,
        meta: crate::sessions::SessionMeta::default(),
    });
    Ok(LoadedSession {
        header,
        messages,
        sub_msgs,
        meta,
        warnings,
    })
}

pub fn load_meta(dir: &Path) -> Result<crate::tree::MetaRecord, StorageError> {
    let path = meta_path(dir);
    let data = fs::read(&path).map_err(|e| StorageError::NotFound(format!("meta: {e}")))?;
    let record = serde_json::from_slice(&data)?;
    Ok(record)
}

pub fn init_header(session_id: &str, cwd: &str, created_at: u64) -> Header {
    Header {
        version: LOG_VERSION,
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
        created_at,
        parent_session_id: None,
        created_from_node_id: None,
    }
}

pub fn next_message(
    parent: Option<NodeRef>,
    role: Role,
    content: Vec<Box<RawValue>>,
    timestamp: u64,
) -> MessageNode {
    MessageNode {
        id: MessageId::new(),
        parent_id: parent,
        role,
        content,
        timestamp,
        run_id: None,
        interrupted: false,
        hidden: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_lock_yields_reader() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let id = "test-session";

        let first = open(base, id);
        assert!(matches!(first, OpenResult::Writer(_)));
        let _first = match first {
            OpenResult::Writer(w) => w,
            _ => unreachable!(),
        };

        let second = open(base, id);
        assert!(matches!(second, OpenResult::Reader(_)));
    }

    #[test]
    fn reader_has_no_append_api() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "reader-only";
        let first = open(tmp.path(), id);
        let _first = match first {
            OpenResult::Writer(w) => w,
            _ => unreachable!(),
        };
        let second = open(tmp.path(), id);
        let reader = match second {
            OpenResult::Reader(r) => r,
            _ => unreachable!(),
        };
        let _reader_ref: &SessionReader = &reader;
    }

    #[test]
    fn append_and_reload_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "round-trip";
        let mut w = match open(tmp.path(), id) {
            OpenResult::Writer(w) => w,
            _ => unreachable!(),
        };

        let header = init_header(id, "/cwd", 100);
        let line = serde_json::to_string(&TreeRecord::Header(header)).unwrap();
        use std::io::Write as _;
        w.log_file.write_all(line.as_bytes()).unwrap();
        w.log_file.write_all(b"\n").unwrap();

        let node = next_message(None, Role::User, Vec::new(), 200);
        w.append_message(node.clone()).unwrap();
        w.sync().unwrap();
        drop(w);

        let reopened = open(tmp.path(), id);
        match reopened {
            OpenResult::Writer(w2) => {
                assert_eq!(w2.loaded.messages.len(), 1);
                assert_eq!(w2.loaded.header.session_id, id);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn mid_file_bad_line_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "bad-line";
        let dir = crate::paths::session_dir(tmp.path(), id);
        fs::create_dir_all(&dir).unwrap();
        let log = crate::paths::log_path(&dir);
        let header = serde_json::to_string(&TreeRecord::Header(init_header(id, "/c", 1))).unwrap();
        let node = next_message(None, Role::User, Vec::new(), 2);
        let node_line = serde_json::to_string(&TreeRecord::Message(node)).unwrap();
        let content = format!("{header}\n{node_line}\n{{\"t\":\"message\" INVALID JSON\n");
        std::fs::write(&log, content).unwrap();

        let loaded = load_folder(&dir, id).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert!(!loaded.warnings.is_empty());
    }

    #[test]
    fn unknown_tag_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "unknown-tag";
        let dir = crate::paths::session_dir(tmp.path(), id);
        fs::create_dir_all(&dir).unwrap();
        let log = crate::paths::log_path(&dir);
        let header = serde_json::to_string(&TreeRecord::Header(init_header(id, "/c", 1))).unwrap();
        let content = format!("{header}\n{{\"t\":\"future_record\",\"x\":42}}\n");
        std::fs::write(&log, content).unwrap();

        let loaded = load_folder(&dir, id).unwrap();
        assert_eq!(loaded.messages.len(), 0);
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn meta_json_atomic_round_trip() {
        use crate::tree::MetaRecord;
        let tmp = tempfile::tempdir().unwrap();
        let id = "meta-rt";
        let dir = crate::paths::session_dir(tmp.path(), id);
        fs::create_dir_all(&dir).unwrap();
        let meta = MetaRecord {
            title: "Test Title".into(),
            cwd: "/project".into(),
            model: "test-model".into(),
            updated_at: 999,
            migration: None,
            meta: crate::sessions::SessionMeta::default(),
        };
        let json = serde_json::to_vec_pretty(&meta).unwrap();
        atomic_write(&meta_path(&dir), &json).unwrap();
        let loaded = load_meta(&dir).unwrap();
        assert_eq!(loaded.title, "Test Title");
        assert_eq!(loaded.model, "test-model");
        assert_eq!(loaded.updated_at, 999);
    }
}
