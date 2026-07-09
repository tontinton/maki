//! Per-session folder reader/writer: `SessionReader`/`SessionWriter` capability
//! split (§A.0(5), §13), and the linear C1 load path.
//!
//! Acquiring the `fs4` exclusive lock on the sentinel `sessions/<id>/lock` is
//! the ONLY way to construct `SessionWriter`; lock contention (or fsync
//! failure, or unsupported version) yields a `SessionReader`, which has NO
//! append methods. Read-only mode is the absence of capability, not a flag.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;
use serde_json::value::RawValue;
use tracing::warn;

use crate::paths::{lock_path, log_path, meta_path, renders_path, session_dir};
use crate::renders::{self, RenderStore};
use crate::sessions::SessionError;
use crate::tree::TreeEvent::{Append, Fork, Navigate, Summary};
use crate::tree::{
    AppendKind, Flavor, Header, LeafId, LeafRecord, MessageId, MessageNode, NodeRef, OrderedRecord,
    Position, Role, SessionTree, SummaryRecord, ToolUseId, TreeEvent, TreeMutation, TreeNode,
    TreeRecord,
};
use crate::{StorageError, atomic_write, now_epoch};

const LOG_VERSION: u32 = 3;

/// C1 linear model: a chain of message nodes, sub_msgs (raw), meta, warnings.
/// Tree records (leaves/summaries/labels) hang off the side; `build_session_tree`
/// assembles the §A.5 domain model from them.
pub struct LoadedSession {
    pub header: Header,
    pub messages: Vec<MessageNode>,
    pub sub_msgs: Vec<crate::tree::SubMsgRecord>,
    pub leaves: Vec<LeafRecord>,
    pub summaries: Vec<SummaryRecord>,
    pub labels: Vec<crate::tree::LabelRecord>,
    pub order: Vec<OrderedRecord>,
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
    nodes: HashMap<NodeRef, TreeNode>,
    order: Vec<OrderedRecord>,
    flavors: HashMap<NodeRef, Flavor>,
}

impl SessionWriter {
    pub fn session_id(&self) -> &str {
        &self.loaded.header.session_id
    }

    /// `<base>/sessions/` — where fork installs the new session folder (§A.8).
    pub fn sessions_base(&self) -> &Path {
        self.session_dir.parent().unwrap_or(&self.session_dir)
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
        let nref = NodeRef::Msg(id.clone());
        self.flavors
            .insert(nref.clone(), SessionTree::node_flavor(&node));
        self.order.push(OrderedRecord::Node(nref.clone()));
        self.nodes.insert(nref, TreeNode::Message(node.clone()));
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

    pub fn append_leaf(&mut self, record: LeafRecord) -> Result<(), StorageError> {
        if self.unclean {
            return Err(unclean_error());
        }
        let mut buf = serde_json::to_vec(&TreeRecord::Leaf(record.clone()))?;
        buf.push(b'\n');
        self.log_file.write_all(&buf)?;
        self.order.push(OrderedRecord::Leaf {
            target: record.target_node_id.clone(),
        });
        self.loaded.leaves.push(record);
        Ok(())
    }

    pub fn append_summary(&mut self, record: SummaryRecord) -> Result<(), StorageError> {
        if self.unclean {
            return Err(unclean_error());
        }
        let mut buf = serde_json::to_vec(&TreeRecord::Summary(record.clone()))?;
        buf.push(b'\n');
        self.log_file.write_all(&buf)?;
        let nref = NodeRef::Sum(record.id.clone());
        self.order.push(OrderedRecord::Node(nref.clone()));
        self.nodes.insert(nref, TreeNode::Summary(record.clone()));
        self.loaded.summaries.push(record);
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

    /// Persist the session header as the first `log.jsonl` line (§14). Idempotent
    /// for a brand-new session opened via `open`; the header is the only record
    /// written before the first mutation.
    pub fn write_header(&mut self) -> Result<(), StorageError> {
        if self.unclean {
            return Err(unclean_error());
        }
        let mut buf = serde_json::to_vec(&TreeRecord::Header(self.loaded.header.clone()))?;
        buf.push(b'\n');
        self.log_file.write_all(&buf)?;
        Ok(())
    }

    pub fn sync(&mut self) -> Result<(), StorageError> {
        if self.unclean {
            return Err(unclean_error());
        }
        if let Err(e) = self.log_file.sync_data() {
            self.downgrade(e);
            return Err(unclean_error());
        }
        if let Err(e) = self.renders.sync_file() {
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

    pub fn nodes(&self) -> &HashMap<NodeRef, TreeNode> {
        &self.nodes
    }

    pub fn order(&self) -> &[OrderedRecord] {
        &self.order
    }

    pub fn flavors(&self) -> &HashMap<NodeRef, Flavor> {
        &self.flavors
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
    Error(SessionError),
}

/// Open a session folder. Tries to acquire the writer lock; on contention (or
/// fsync failure) returns a `SessionReader`.
pub fn open(base: &Path, id: &str) -> OpenResult {
    let dir = session_dir(base, id);
    let mut is_new = false;
    let loaded = match load_folder(&dir, id) {
        Ok(l) => l,
        Err(StorageError::NotFound(_)) => {
            is_new = true;
            LoadedSession {
                header: init_header(id, "", now_epoch()),
                messages: Vec::new(),
                sub_msgs: Vec::new(),
                leaves: Vec::new(),
                summaries: Vec::new(),
                labels: Vec::new(),
                order: Vec::new(),
                meta: crate::tree::MetaRecord {
                    title: String::new(),
                    cwd: String::new(),
                    model: String::new(),
                    updated_at: now_epoch(),
                    migration: None,
                    meta: crate::sessions::SessionMeta::default(),
                },
                warnings: Vec::new(),
            }
        }
        Err(e) => return OpenResult::Error(SessionError::Storage(e)),
    };
    if loaded.header.version > LOG_VERSION {
        return OpenResult::Unsupported(loaded.header.version);
    }
    // §A.5 open step 2: cycle check here is what lets `fold` be infallible.
    let (nodes, order, flavors) = tree_state_from_loaded(&loaded);
    if let Err(e) = check_cycles(&nodes) {
        return OpenResult::Error(e);
    }
    if fs::create_dir_all(&dir).is_err() {
        return OpenResult::Error(SessionError::Storage(StorageError::NotFound(id.into())));
    }
    let lock = match File::create(lock_path(&dir)) {
        Ok(f) => f,
        Err(e) => return OpenResult::Error(SessionError::Storage(StorageError::Io(e))),
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
            return OpenResult::Error(SessionError::Storage(StorageError::Io(e)));
        }
    };
    let renders = match RenderStore::open(&renders_path(&dir)) {
        Ok(r) => r,
        Err(e) => {
            let _ = FileExt::unlock(&lock);
            return OpenResult::Error(SessionError::Storage(StorageError::Io(e)));
        }
    };
    let compressor = match renders::new_compressor() {
        Ok(c) => c,
        Err(e) => {
            let _ = FileExt::unlock(&lock);
            return OpenResult::Error(SessionError::Storage(StorageError::Io(e)));
        }
    };
    let mut writer = SessionWriter {
        session_dir: dir,
        loaded,
        log_file,
        lock,
        renders,
        compressor,
        unclean: false,
        nodes,
        order,
        flavors,
    };
    // A brand-new session has an in-memory header but no log line yet (§14);
    // persist it so a later load_folder finds the header.
    if is_new && let Err(e) = writer.write_header() {
        writer.downgrade(std::io::Error::other(format!("header write: {e}")));
    }
    OpenResult::Writer(writer)
}

pub fn load_folder(dir: &Path, id: &str) -> Result<LoadedSession, StorageError> {
    let log = log_path(dir);
    let mut warnings = Vec::new();
    let mut header: Option<Header> = None;
    let mut messages = Vec::new();
    let mut sub_msgs = Vec::new();
    let mut leaves = Vec::new();
    let mut summaries = Vec::new();
    let mut labels = Vec::new();
    let mut order: Vec<OrderedRecord> = Vec::new();

    let file = File::open(&log).map_err(|e| StorageError::NotFound(format!("{id}: {e}")))?;
    let reader = BufReader::new(file);
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        match crate::tree::parse_line(&line) {
            Ok(Some(TreeRecord::Header(h))) => header = Some(h),
            Ok(Some(TreeRecord::Message(m))) => {
                order.push(OrderedRecord::Node(NodeRef::Msg(m.id.clone())));
                messages.push(m);
            }
            Ok(Some(TreeRecord::Leaf(l))) => {
                order.push(OrderedRecord::Leaf {
                    target: l.target_node_id.clone(),
                });
                leaves.push(l);
            }
            Ok(Some(TreeRecord::Summary(s))) => {
                order.push(OrderedRecord::Node(NodeRef::Sum(s.id.clone())));
                summaries.push(s);
            }
            Ok(Some(TreeRecord::Label(l))) => labels.push(l),
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
        leaves,
        summaries,
        labels,
        order,
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

fn tree_state_from_loaded(
    loaded: &LoadedSession,
) -> (
    HashMap<NodeRef, TreeNode>,
    Vec<OrderedRecord>,
    HashMap<NodeRef, Flavor>,
) {
    let mut nodes = HashMap::new();
    let mut flavors = HashMap::new();
    for m in &loaded.messages {
        let nref = NodeRef::Msg(m.id.clone());
        flavors.insert(nref.clone(), SessionTree::node_flavor(m));
        nodes.insert(nref, TreeNode::Message(m.clone()));
    }
    for s in &loaded.summaries {
        nodes.insert(NodeRef::Sum(s.id.clone()), TreeNode::Summary(s.clone()));
    }
    (nodes, loaded.order.clone(), flavors)
}

/// Assemble the §A.5 domain model from a load: nodes/order/flavors, the leaf
/// resolved via the leaf rule, and the open-time cycle check.
pub fn build_session_tree(loaded: &LoadedSession) -> Result<SessionTree, SessionError> {
    let (nodes, order, flavors) = tree_state_from_loaded(loaded);
    check_cycles(&nodes)?;
    let leaf = active_leaf(&order, &nodes);
    Ok(SessionTree {
        nodes,
        order,
        leaf,
        labels: loaded.labels.clone(),
        sub_msgs: loaded.sub_msgs.clone(),
        flavors,
    })
}

/// §A.5 open step 2: one O(n) parent-walk over `nodes`. A cycle is the one
/// unservable shape → `CorruptTree` at open (what lets `fold` be infallible).
fn check_cycles(nodes: &HashMap<NodeRef, TreeNode>) -> Result<(), SessionError> {
    for start in nodes.keys() {
        let mut cur = Some(start.clone());
        let mut seen = HashSet::new();
        while let Some(nref) = cur {
            if !seen.insert(nref.clone()) {
                return Err(SessionError::CorruptTree {
                    cycle_at: start.to_string(),
                });
            }
            cur = nodes.get(&nref).and_then(TreeNode::parent_id);
        }
    }
    Ok(())
}

/// The leaf rule (§4/§A.5), as code. Reverse append order; first Leaf wins by
/// its target (skipping unresolved targets with a warn); first Node wins by its
/// id; else Root.
pub fn active_leaf(order: &[OrderedRecord], nodes: &HashMap<NodeRef, TreeNode>) -> Position {
    for r in order.iter().rev() {
        match r {
            OrderedRecord::Leaf { target: None } => return Position::Root,
            OrderedRecord::Leaf { target: Some(t) } if nodes.contains_key(t) => {
                return Position::At(t.clone());
            }
            OrderedRecord::Leaf { target: Some(t) } => {
                warn!(target = %t, "leaf target missing; skipping");
            }
            OrderedRecord::Node(nref) => return Position::At(nref.clone()),
        }
    }
    Position::Root
}

/// Undo-of-rewind (§A.5): the position before the last Leaf record, recovered
/// by running `active_leaf` over the prefix ending just before it. Only called
/// when `order.last()` is itself a `Leaf`, so `i` always exists.
pub fn position_before_last_leaf(
    order: &[OrderedRecord],
    nodes: &HashMap<NodeRef, TreeNode>,
) -> Position {
    let i = order
        .iter()
        .rposition(|r| matches!(r, OrderedRecord::Leaf { .. }))
        .expect("position_before_last_leaf requires order to end with a Leaf");
    active_leaf(&order[..i], nodes)
}

/// §13 writer: owns a `SessionWriter`, drains an unbounded `flume<TreeMutation>`
/// (no coalescing — every mutation durable), emits `TreeEvent`s after the write
/// and before the batched fsync, and acks `Barrier` once fsync succeeds. Never
/// blocks on or calls a model; fsync failure downgrades the writer (§13).
pub struct TreeWriter {
    writer: SessionWriter,
    rx: flume::Receiver<TreeMutation>,
    events: flume::Sender<TreeEvent>,
    event_rx: flume::Receiver<TreeEvent>,
}

impl TreeWriter {
    pub fn new(writer: SessionWriter, rx: flume::Receiver<TreeMutation>) -> Self {
        let (events, event_rx) = flume::unbounded();
        Self {
            writer,
            rx,
            events,
            event_rx,
        }
    }

    pub fn events(&self) -> flume::Receiver<TreeEvent> {
        self.event_rx.clone()
    }

    pub fn run(mut self) {
        while let Ok(mutation) = self.rx.recv() {
            if self.writer.unclean {
                continue;
            }
            match mutation {
                TreeMutation::AppendMessage(node) => {
                    let nref = NodeRef::Msg(node.id.clone());
                    if self.writer.append_message(node).is_ok() {
                        self.emit(Append {
                            node_id: Some(nref),
                            kind: AppendKind::Message,
                        });
                    }
                }
                TreeMutation::AppendSubMsg(record) => {
                    if self.writer.append_sub_msg(record).is_ok() {
                        self.emit(Append {
                            node_id: None,
                            kind: AppendKind::SubMsg,
                        });
                    }
                }
                TreeMutation::AppendRender { tool_use_id, frame } => {
                    if self.writer.append_render(&tool_use_id, &frame).is_ok() {
                        self.emit(Append {
                            node_id: None,
                            kind: AppendKind::Render,
                        });
                    }
                }
                TreeMutation::SetMeta(meta) => {
                    self.writer.loaded.meta = meta;
                    if self.writer.write_meta().is_ok() {
                        self.emit(Append {
                            node_id: None,
                            kind: AppendKind::Meta,
                        });
                    }
                }
                TreeMutation::Rewind { target } => {
                    let old = active_leaf(&self.writer.order, &self.writer.nodes);
                    let record = LeafRecord {
                        id: LeafId::new(),
                        target_node_id: target.node_ref().cloned(),
                    };
                    if self.writer.append_leaf(record).is_ok() {
                        let new = active_leaf(&self.writer.order, &self.writer.nodes);
                        self.emit(Navigate {
                            old_leaf: old,
                            new_leaf: new,
                        });
                    }
                }
                TreeMutation::AppendSummary(record) => {
                    let sid = record.id.clone();
                    let kind = record.kind.clone();
                    if self.writer.append_summary(record).is_ok() {
                        self.emit(Summary { node_id: sid, kind });
                    }
                }
                TreeMutation::Barrier(ack) => {
                    let _ = self.writer.sync();
                    let _ = ack.send(());
                }
                TreeMutation::Fork {
                    new_session_id,
                    from_node_id,
                    ack,
                } => {
                    let _ = self.writer.sync();
                    let sessions_base = self.writer.sessions_base().to_path_buf();
                    let result = crate::fork::fork_to(
                        &self.writer.loaded,
                        &mut self.writer.renders,
                        &sessions_base,
                        &new_session_id,
                        &from_node_id,
                    );
                    match result {
                        Ok(parent_title) => {
                            self.emit(Fork {
                                new_session_id: new_session_id.clone(),
                                parent_title: parent_title.clone(),
                            });
                            let _ = ack.send(Ok(crate::tree::ForkResult {
                                new_session_id,
                                parent_title,
                            }));
                        }
                        Err(e) => {
                            let _ = ack.send(Err(e.to_string()));
                        }
                    }
                }
            }
        }
    }

    fn emit(&self, event: TreeEvent) {
        let _ = self.events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{SummaryId, SummaryKind};

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

    fn write_log(dir: &Path, id: &str, lines: &[String]) {
        let log = crate::paths::log_path(&crate::paths::session_dir(dir, id));
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, lines.join("\n") + "\n").unwrap();
    }

    fn load_tree(dir: &Path, id: &str) -> SessionTree {
        let loaded = load_folder(&crate::paths::session_dir(dir, id), id).unwrap();
        build_session_tree(&loaded).unwrap()
    }

    fn msg_line(parent: Option<NodeRef>, role: Role, ts: u64) -> (String, MessageId) {
        let node = next_message(parent, role, Vec::new(), ts);
        let id = node.id.clone();
        (
            serde_json::to_string(&TreeRecord::Message(node)).unwrap(),
            id,
        )
    }

    fn leaf_line(target: Option<NodeRef>) -> String {
        let record = LeafRecord {
            id: LeafId::new(),
            target_node_id: target,
        };
        serde_json::to_string(&TreeRecord::Leaf(record)).unwrap()
    }

    #[test]
    fn active_leaf_last_message_wins() {
        let m1 = MessageId::new();
        let m2 = MessageId::new();
        let mut nodes = HashMap::new();
        nodes.insert(NodeRef::Msg(m1.clone()), TreeNode::Message(node_only(&m1)));
        nodes.insert(NodeRef::Msg(m2.clone()), TreeNode::Message(node_only(&m2)));
        let order = vec![
            OrderedRecord::Node(NodeRef::Msg(m1)),
            OrderedRecord::Node(NodeRef::Msg(m2.clone())),
        ];
        assert_eq!(active_leaf(&order, &nodes), Position::At(NodeRef::Msg(m2)));
    }

    #[test]
    fn active_leaf_leaf_record_overrides_last_message() {
        let m1 = MessageId::new();
        let m2 = MessageId::new();
        let mut nodes = HashMap::new();
        nodes.insert(NodeRef::Msg(m1.clone()), TreeNode::Message(node_only(&m1)));
        nodes.insert(NodeRef::Msg(m2.clone()), TreeNode::Message(node_only(&m2)));
        let order = vec![
            OrderedRecord::Node(NodeRef::Msg(m1.clone())),
            OrderedRecord::Node(NodeRef::Msg(m2)),
            OrderedRecord::Leaf {
                target: Some(NodeRef::Msg(m1.clone())),
            },
        ];
        assert_eq!(active_leaf(&order, &nodes), Position::At(NodeRef::Msg(m1)));
    }

    #[test]
    fn active_leaf_root_target_returns_root() {
        let m1 = MessageId::new();
        let mut nodes = HashMap::new();
        nodes.insert(NodeRef::Msg(m1.clone()), TreeNode::Message(node_only(&m1)));
        let order = vec![
            OrderedRecord::Node(NodeRef::Msg(m1)),
            OrderedRecord::Leaf { target: None },
        ];
        assert_eq!(active_leaf(&order, &nodes), Position::Root);
    }

    #[test]
    fn active_leaf_skips_dangling_target() {
        let m1 = MessageId::new();
        let dangling = NodeRef::Msg(MessageId::new());
        let mut nodes = HashMap::new();
        nodes.insert(NodeRef::Msg(m1.clone()), TreeNode::Message(node_only(&m1)));
        let order = vec![
            OrderedRecord::Node(NodeRef::Msg(m1.clone())),
            OrderedRecord::Leaf {
                target: Some(dangling),
            },
            OrderedRecord::Node(NodeRef::Msg(m1.clone())),
        ];
        assert_eq!(active_leaf(&order, &nodes), Position::At(NodeRef::Msg(m1)));
    }

    #[test]
    fn active_leaf_summary_advances_leaf() {
        let s = SummaryId::new();
        let m1 = MessageId::new();
        let mut nodes = HashMap::new();
        nodes.insert(NodeRef::Msg(m1.clone()), TreeNode::Message(node_only(&m1)));
        nodes.insert(
            NodeRef::Sum(s.clone()),
            TreeNode::Summary(compaction_summary(NodeRef::Msg(m1.clone()), s.clone())),
        );
        let order = vec![
            OrderedRecord::Node(NodeRef::Msg(m1)),
            OrderedRecord::Node(NodeRef::Sum(s.clone())),
        ];
        assert_eq!(active_leaf(&order, &nodes), Position::At(NodeRef::Sum(s)));
    }

    #[test]
    fn position_before_last_leaf_restores_pre_rewind_tip() {
        let m1 = MessageId::new();
        let m2 = MessageId::new();
        let mut nodes = HashMap::new();
        nodes.insert(NodeRef::Msg(m1.clone()), TreeNode::Message(node_only(&m1)));
        nodes.insert(NodeRef::Msg(m2.clone()), TreeNode::Message(node_only(&m2)));
        let order = vec![
            OrderedRecord::Node(NodeRef::Msg(m1.clone())),
            OrderedRecord::Node(NodeRef::Msg(m2.clone())),
            OrderedRecord::Leaf {
                target: Some(NodeRef::Msg(m1.clone())),
            },
        ];
        assert_eq!(
            position_before_last_leaf(&order, &nodes),
            Position::At(NodeRef::Msg(m2))
        );
    }

    #[test]
    fn leaf_rule_rewind_then_push_resolves_to_push() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "rewind-push-resume";
        let (m1_line, m1) = msg_line(None, Role::User, 1);
        let (m2_line, _m2) = msg_line(Some(NodeRef::Msg(m1.clone())), Role::Assistant, 2);
        let (m3_line, m3) = msg_line(Some(NodeRef::Msg(m1.clone())), Role::User, 3);
        let rewind = leaf_line(Some(NodeRef::Msg(m1)));
        let header = serde_json::to_string(&TreeRecord::Header(init_header(id, "/c", 0))).unwrap();
        write_log(tmp.path(), id, &[header, m1_line, m2_line, rewind, m3_line]);
        let tree = load_tree(tmp.path(), id);
        assert_eq!(tree.leaf, Position::At(NodeRef::Msg(m3)));
    }

    #[test]
    fn leaf_rule_summary_append_advances_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "summary-advances";
        let (m1_line, m1) = msg_line(None, Role::User, 1);
        let s = compaction_summary(NodeRef::Msg(m1), SummaryId::new());
        let s_id = s.id.clone();
        let s_line = serde_json::to_string(&TreeRecord::Summary(s)).unwrap();
        let header = serde_json::to_string(&TreeRecord::Header(init_header(id, "/c", 0))).unwrap();
        write_log(tmp.path(), id, &[header, m1_line, s_line]);
        let tree = load_tree(tmp.path(), id);
        assert_eq!(tree.leaf, Position::At(NodeRef::Sum(s_id)));
    }

    #[test]
    fn leaf_rule_undo_of_first_rewind_restores_tip() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "undo-first-rewind";
        let (m1_line, m1) = msg_line(None, Role::User, 1);
        let (m2_line, m2) = msg_line(Some(NodeRef::Msg(m1.clone())), Role::Assistant, 2);
        let rewind = leaf_line(Some(NodeRef::Msg(m1.clone())));
        let header = serde_json::to_string(&TreeRecord::Header(init_header(id, "/c", 0))).unwrap();
        write_log(tmp.path(), id, &[header, m1_line, m2_line, rewind]);
        let tree = load_tree(tmp.path(), id);
        assert_eq!(tree.leaf, Position::At(NodeRef::Msg(m1)));
        assert_eq!(
            position_before_last_leaf(&tree.order, &tree.nodes),
            Position::At(NodeRef::Msg(m2))
        );
    }

    #[test]
    fn leaf_rule_dangling_leaf_target_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "dangling-leaf";
        let (m1_line, m1) = msg_line(None, Role::User, 1);
        let dangling = NodeRef::Msg(MessageId::new());
        let rewind = leaf_line(Some(dangling));
        let header = serde_json::to_string(&TreeRecord::Header(init_header(id, "/c", 0))).unwrap();
        // Duplicate m1 line so the dangling leaf does not sit at the tail.
        write_log(
            tmp.path(),
            id,
            &[header.clone(), m1_line.clone(), rewind, m1_line],
        );
        let tree = load_tree(tmp.path(), id);
        assert_eq!(tree.leaf, Position::At(NodeRef::Msg(m1)));
    }

    #[test]
    fn cycle_detected_at_open() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "cycle";
        let m1 = MessageId::new();
        let m2 = MessageId::new();
        let n1 = MessageNode {
            id: m1.clone(),
            parent_id: Some(NodeRef::Msg(m2.clone())),
            role: Role::User,
            content: Vec::new(),
            timestamp: 1,
            run_id: None,
            interrupted: false,
            hidden: false,
        };
        let n2 = MessageNode {
            id: m2.clone(),
            parent_id: Some(NodeRef::Msg(m1.clone())),
            role: Role::Assistant,
            content: Vec::new(),
            timestamp: 2,
            run_id: None,
            interrupted: false,
            hidden: false,
        };
        let header = serde_json::to_string(&TreeRecord::Header(init_header(id, "/c", 0))).unwrap();
        let l1 = serde_json::to_string(&TreeRecord::Message(n1)).unwrap();
        let l2 = serde_json::to_string(&TreeRecord::Message(n2)).unwrap();
        write_log(tmp.path(), id, &[header, l1, l2]);
        let loaded = load_folder(&crate::paths::session_dir(tmp.path(), id), id).unwrap();
        let err = build_session_tree(&loaded).unwrap_err();
        assert!(matches!(err, SessionError::CorruptTree { .. }));
    }

    #[test]
    fn cycle_in_open_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "open-cycle";
        let m1 = MessageId::new();
        let m2 = MessageId::new();
        let n1 = MessageNode {
            id: m1.clone(),
            parent_id: Some(NodeRef::Msg(m2.clone())),
            role: Role::User,
            content: Vec::new(),
            timestamp: 1,
            run_id: None,
            interrupted: false,
            hidden: false,
        };
        let n2 = MessageNode {
            id: m2.clone(),
            parent_id: Some(NodeRef::Msg(m1)),
            role: Role::Assistant,
            content: Vec::new(),
            timestamp: 2,
            run_id: None,
            interrupted: false,
            hidden: false,
        };
        let dir = crate::paths::session_dir(tmp.path(), id);
        fs::create_dir_all(&dir).unwrap();
        let header = serde_json::to_string(&TreeRecord::Header(init_header(id, "/c", 0))).unwrap();
        let l1 = serde_json::to_string(&TreeRecord::Message(n1)).unwrap();
        let l2 = serde_json::to_string(&TreeRecord::Message(n2)).unwrap();
        write_log(tmp.path(), id, &[header, l1, l2]);
        assert!(matches!(
            open(tmp.path(), id),
            OpenResult::Error(SessionError::CorruptTree { .. })
        ));
    }

    #[test]
    fn broken_parent_chain_serves_reachable_suffix() {
        let m1 = MessageId::new();
        let m2 = MessageId::new();
        let mut nodes = HashMap::new();
        nodes.insert(NodeRef::Msg(m1.clone()), TreeNode::Message(node_only(&m1)));
        nodes.insert(
            NodeRef::Msg(m2.clone()),
            TreeNode::Message(MessageNode {
                id: m2.clone(),
                parent_id: Some(NodeRef::Msg(MessageId::new())),
                role: Role::User,
                content: Vec::new(),
                timestamp: 2,
                run_id: None,
                interrupted: false,
                hidden: false,
            }),
        );
        let order = vec![
            OrderedRecord::Node(NodeRef::Msg(m1)),
            OrderedRecord::Node(NodeRef::Msg(m2.clone())),
        ];
        // No panic: active_leaf resolves; the broken parent (m2.parent absent)
        // is a walk boundary for fold, not a load failure.
        assert_eq!(active_leaf(&order, &nodes), Position::At(NodeRef::Msg(m2)));
    }

    fn node_only(id: &MessageId) -> MessageNode {
        MessageNode {
            id: id.clone(),
            parent_id: None,
            role: Role::User,
            content: Vec::new(),
            timestamp: 0,
            run_id: None,
            interrupted: false,
            hidden: false,
        }
    }

    fn compaction_summary(parent: NodeRef, sid: SummaryId) -> SummaryRecord {
        SummaryRecord {
            id: sid,
            parent_id: parent,
            narrative: "compaction narrative".into(),
            kind: SummaryKind::Compaction {
                fold_to_id: MessageId::new(),
            },
            read_files: Vec::new(),
            modified_files: Vec::new(),
        }
    }
}
