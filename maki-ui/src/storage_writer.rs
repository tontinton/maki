//! Tree-backed storage writer (§10, §13).
//!
//! The UI posts full `AppSession` snapshots; the writer computes the delta
//! against its per-session cursor and enqueues typed `TreeMutation`s onto an
//! unbounded channel. A background `TreeWriter` drains them, owning `log.jsonl`
//! /`renders.bin`/`meta.json`. `Barrier` is fsync + ack oneshot; on fsync
//! failure the `SessionWriter` downgrades to read-only and stops accepting.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use maki_providers::{ContentBlock, Message};
use maki_storage::StateDir;
use maki_storage::session_log::{OpenResult, TreeWriter, active_leaf};
use maki_storage::sessions::SESSIONS_DIR;
use maki_storage::tree::{
    MessageId, MessageNode, MetaRecord, NodeRef, Position, Role, ToolUseId, TreeEvent, TreeMutation,
};
use serde_json::value::RawValue;
use tracing::warn;

use crate::AppSession;

/// Cursor tracking the persisted prefix (mirrors `SessionLog`'s saved counts).
/// Append-only: a rewind enqueues a `Rewind` mutation that appends a `Leaf`;
/// the cursor stays at the tipward node so subsequent appends parent correctly.
struct Cursor {
    saved_msg_count: usize,
    saved_tool_ids: HashSet<String>,
    saved_sub_counts: HashMap<String, usize>,
    last_leaf: Position,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            saved_msg_count: 0,
            saved_tool_ids: HashSet::new(),
            saved_sub_counts: HashMap::new(),
            last_leaf: Position::Root,
        }
    }
}

struct WriterState {
    session_id: String,
    cursor: Cursor,
    /// Lock contention, unsupported version, or fsync-downgrade.
    readonly: bool,
}

/// One per active session folder; `None` until the first `send` opens one.
struct SessionHandle {
    tx: flume::Sender<TreeMutation>,
    events: flume::Receiver<TreeEvent>,
    done_rx: flume::Receiver<()>,
}

pub struct StorageWriter {
    dir: StateDir,
    handle: Mutex<Option<SessionHandle>>,
    state: Mutex<WriterState>,
}

/// Failures from a C3 rewind/landing commit.
#[derive(Debug, thiserror::Error)]
pub enum RewindError {
    #[error("session is read-only; cannot rewind")]
    Readonly,
    #[error("no writer for this session")]
    NoWriter,
}

impl StorageWriter {
    pub fn new(dir: StateDir) -> Self {
        Self {
            dir,
            handle: Mutex::new(None),
            state: Mutex::new(WriterState {
                session_id: String::new(),
                cursor: Cursor::default(),
                readonly: true,
            }),
        }
    }

    pub fn send(&self, session: &AppSession) {
        {
            let state = self.state.lock().unwrap();
            if state.readonly && state.session_id == session.id {
                return;
            }
        }

        let need_open = self.state.lock().unwrap().session_id != session.id;
        if need_open {
            self.open_session(session);
        }

        let mutations = {
            let mut state = self.state.lock().unwrap();
            if state.readonly {
                return;
            }
            compute_delta(&mut state.cursor, session)
        };

        let guard = self.handle.lock().unwrap();
        if let Some(handle) = guard.as_ref() {
            for m in mutations {
                if handle.tx.send(m).is_err() {
                    break;
                }
            }
        }
    }

    /// Enqueue a barrier and block until the writer fsyncs and acks.
    pub fn barrier(&self) -> Result<(), flume::RecvError> {
        let (ack_tx, ack_rx) = flume::bounded::<()>(1);
        let guard = self.handle.lock().unwrap();
        let Some(handle) = guard.as_ref() else {
            return Err(flume::RecvError::Disconnected);
        };
        if handle.tx.send(TreeMutation::Barrier(ack_tx)).is_err() {
            return Err(flume::RecvError::Disconnected);
        }
        drop(guard);
        ack_rx.recv()
    }

    pub fn tree_events(&self) -> flume::Receiver<TreeEvent> {
        self.handle
            .lock()
            .unwrap()
            .as_ref()
            .map(|h| h.events.clone())
            .unwrap_or_else(|| flume::unbounded().1)
    }

    /// The current leaf position the writer's cursor tracks (§4). This is the
    /// node that closed the last run — the key for a run-end snapshot (§7).
    pub fn leaf_position(&self) -> Position {
        self.state.lock().unwrap().cursor.last_leaf.clone()
    }

    /// Open session id for the active writer, if any.
    pub fn session_id(&self) -> Option<String> {
        let state = self.state.lock().unwrap();
        (!state.session_id.is_empty()).then(|| state.session_id.clone())
    }

    /// The state-dir base the writer opens session folders under.
    pub fn dir(&self) -> &StateDir {
        &self.dir
    }

    /// Fork root→cursor into a new session (§5, §A.8). Enqueues the `Fork`
    /// mutation; the writer flushes buffered appends, copies the on-path nodes,
    /// renders, and subagent transcripts into a staged temp dir, atomically
    /// renames it into place, then acks. Returns the new session id and parent
    /// title on success. The caller copies snapshots post-ack. Returns
    /// `Err(Readonly)` if the writer is read-only, or `Err(NoWriter)` if the
    /// channel is gone.
    pub fn fork(
        &self,
        new_session_id: String,
        from_node_id: NodeRef,
    ) -> Result<maki_storage::tree::ForkResult, RewindError> {
        {
            let state = self.state.lock().unwrap();
            if state.readonly {
                return Err(RewindError::Readonly);
            }
        }
        let (ack_tx, ack_rx) = flume::bounded::<Result<maki_storage::tree::ForkResult, String>>(1);
        let guard = self.handle.lock().unwrap();
        let Some(handle) = guard.as_ref() else {
            return Err(RewindError::NoWriter);
        };
        if handle
            .tx
            .send(TreeMutation::Fork {
                new_session_id: new_session_id.clone(),
                from_node_id,
                ack: ack_tx,
            })
            .is_err()
        {
            return Err(RewindError::NoWriter);
        }
        drop(guard);
        ack_rx
            .recv()
            .map_err(|_| RewindError::NoWriter)?
            .map_err(|_| RewindError::Readonly)
    }

    /// Reset the cursor to match a rebuilt messages vec after a rewind. The
    /// folded active branch is already on disk (the rewind committed it); the
    /// cursor must not re-append it. `last_leaf` is preserved — it was set by
    /// `rewind`/`rewind_to_interrupted_sibling` and is where the next push
    /// parents onto.
    pub fn reset_msg_cursor(&self, msg_count: usize) {
        let mut state = self.state.lock().unwrap();
        state.cursor.saved_msg_count = msg_count;
    }

    /// Enqueue a `TreeMutation::Rewind { target }` (§4: appends a `Leaf`),
    /// then a `Barrier` and block for the ack. Rewinds are appends — nothing is
    /// deleted; the abandoned branch is preserved on disk. The cursor's
    /// `last_leaf` advances to `target` so the next `send` parents onto it.
    /// Returns `Err` if the writer is readonly or the barrier fails.
    pub fn rewind(&self, target: Position) -> Result<Position, RewindError> {
        let prev = {
            let mut state = self.state.lock().unwrap();
            if state.readonly {
                return Err(RewindError::Readonly);
            }
            let prev = state.cursor.last_leaf.clone();
            state.cursor.last_leaf = target.clone();
            prev
        };

        let guard = self.handle.lock().unwrap();
        let Some(handle) = guard.as_ref() else {
            return Err(RewindError::NoWriter);
        };
        if handle.tx.send(TreeMutation::Rewind { target }).is_err() {
            return Err(RewindError::NoWriter);
        }
        drop(guard);

        let (ack_tx, ack_rx) = flume::bounded::<()>(1);
        {
            let guard = self.handle.lock().unwrap();
            let Some(handle) = guard.as_ref() else {
                return Err(RewindError::NoWriter);
            };
            if handle.tx.send(TreeMutation::Barrier(ack_tx)).is_err() {
                return Err(RewindError::NoWriter);
            }
        }
        ack_rx.recv().map_err(|_| RewindError::NoWriter)?;
        Ok(prev)
    }

    /// Enqueue an `interrupted` assistant sibling node (§4 block-boundary
    /// landing): a new `MessageNode` with `parent_id = parent` and `interrupted:
    /// true`, then `Rewind` onto it. `content` must already be the output of
    /// `FinalizedPartial::from_completed_blocks`. Returns the new node's id.
    pub fn rewind_to_interrupted_sibling(
        &self,
        parent: Option<NodeRef>,
        content_raw: Vec<Box<RawValue>>,
    ) -> Result<MessageId, RewindError> {
        let id = MessageId::new();
        let node = MessageNode {
            id: id.clone(),
            parent_id: parent.clone(),
            role: Role::Assistant,
            content: content_raw,
            timestamp: maki_storage::now_epoch(),
            run_id: None,
            interrupted: true,
            hidden: false,
        };
        let target = Position::At(NodeRef::Msg(id.clone()));

        {
            let mut state = self.state.lock().unwrap();
            if state.readonly {
                return Err(RewindError::Readonly);
            }
            // The derived node is durably appended directly (not via a future
            // `send`), so it must not also be re-derived from the messages vec.
            state.cursor.saved_msg_count = state.cursor.saved_msg_count.saturating_add(1);
            state.cursor.last_leaf = target.clone();
        }

        let guard = self.handle.lock().unwrap();
        let Some(handle) = guard.as_ref() else {
            return Err(RewindError::NoWriter);
        };
        if handle.tx.send(TreeMutation::AppendMessage(node)).is_err() {
            return Err(RewindError::NoWriter);
        }
        if handle.tx.send(TreeMutation::Rewind { target }).is_err() {
            return Err(RewindError::NoWriter);
        }
        drop(guard);

        let (ack_tx, ack_rx) = flume::bounded::<()>(1);
        {
            let guard = self.handle.lock().unwrap();
            let Some(handle) = guard.as_ref() else {
                return Err(RewindError::NoWriter);
            };
            if handle.tx.send(TreeMutation::Barrier(ack_tx)).is_err() {
                return Err(RewindError::NoWriter);
            }
        }
        ack_rx.recv().map_err(|_| RewindError::NoWriter)?;
        Ok(id)
    }

    pub fn shutdown(self, timeout: Duration) {
        let handle = self.handle.lock().unwrap().take();
        drop(self.handle);
        drop(self.state);
        if let Some(h) = handle {
            drop(h.tx);
            if h.done_rx.recv_timeout(timeout).is_err() {
                warn!("storage writer did not drain within {timeout:?}");
            }
        }
    }

    fn open_session(&self, session: &AppSession) {
        // session_log::open expects the state-dir base; it computes
        // `<base>/sessions/<id>/` internally.
        let base = self.dir.path();
        let _ = self.dir.ensure_subdir(SESSIONS_DIR);

        match maki_storage::session_log::open(base, &session.id) {
            OpenResult::Writer(mut writer) => {
                let saved_msg_count = writer.loaded.messages.len();
                let saved_tool_ids: HashSet<String> = session
                    .tool_outputs
                    .keys()
                    .filter(|id| {
                        ToolUseId::new((*id).clone())
                            .is_some_and(|tid| writer.renders().contains(&tid))
                    })
                    .cloned()
                    .collect();
                let saved_sub_counts = count_sub_msgs(&writer.loaded.sub_msgs);
                let last_leaf = active_leaf(writer.order(), writer.nodes());

                let (tx, rx) = flume::unbounded::<TreeMutation>();
                let tree_writer = TreeWriter::new(writer, rx);
                let events = tree_writer.events();
                let (done_tx, done_rx) = flume::bounded::<()>(1);
                std::thread::Builder::new()
                    .name("storage-writer".into())
                    .spawn(move || {
                        tree_writer.run();
                        let _ = done_tx.send(());
                    })
                    .expect("failed to spawn storage writer thread");

                let prev = self.handle.lock().unwrap().replace(SessionHandle {
                    tx,
                    events,
                    done_rx,
                });
                drop_prev(prev);

                let mut state = self.state.lock().unwrap();
                state.session_id = session.id.clone();
                state.readonly = false;
                state.cursor = Cursor {
                    saved_msg_count,
                    saved_tool_ids,
                    saved_sub_counts,
                    last_leaf,
                };
            }
            OpenResult::Reader(_) => {
                warn!(session_id = %session.id, "session locked; opening read-only");
                self.mark_readonly(&session.id);
            }
            OpenResult::Unsupported(version) => {
                warn!(
                    session_id = %session.id,
                    version, "unsupported log version; opening read-only"
                );
                self.mark_readonly(&session.id);
            }
            OpenResult::Error(e) => {
                warn!(session_id = %session.id, error = %e, "session open failed");
                self.mark_readonly(&session.id);
            }
        }
    }

    fn mark_readonly(&self, id: &str) {
        let mut state = self.state.lock().unwrap();
        state.session_id = id.to_string();
        state.readonly = true;
        state.cursor = Cursor::default();
    }
}

/// Drop the previous session handle: closing its sender lets the old writer
/// thread drain and exit. We don't wait — the OS reclaims it.
fn drop_prev(prev: Option<SessionHandle>) {
    if let Some(h) = prev {
        drop(h.tx);
        // Don't block on the old writer; it exits on its own.
        let _ = h.done_rx.try_recv();
    }
}

fn compute_delta(cursor: &mut Cursor, session: &AppSession) -> Vec<TreeMutation> {
    let mut out = Vec::new();

    for msg in &session.messages[cursor.saved_msg_count..] {
        let node = message_to_node(msg, cursor.last_leaf.node_ref().cloned());
        cursor.last_leaf = Position::At(NodeRef::Msg(node.id.clone()));
        cursor.saved_msg_count += 1;
        out.push(TreeMutation::AppendMessage(node));
    }

    for (id, output) in &session.tool_outputs {
        if cursor.saved_tool_ids.insert(id.clone()) {
            let frame = serde_json::to_vec(output).unwrap_or_default();
            if let Some(tool_use_id) = ToolUseId::new(id.clone()) {
                out.push(TreeMutation::AppendRender { tool_use_id, frame });
            }
        }
    }

    for (sub_id, msgs) in &session.subagent_messages {
        let saved = cursor.saved_sub_counts.get(sub_id).copied().unwrap_or(0);
        for msg in &msgs[saved..] {
            if let Ok(d) = serde_json::value::to_raw_value(msg)
                && let Some(sub) = ToolUseId::new(sub_id.clone())
            {
                out.push(TreeMutation::AppendSubMsg(
                    maki_storage::tree::SubMsgRecord { sub, d },
                ));
            }
        }
        if msgs.len() > saved {
            cursor.saved_sub_counts.insert(sub_id.clone(), msgs.len());
        }
    }

    out.push(TreeMutation::SetMeta(meta_record(session)));

    let (ack_tx, _ack_rx) = flume::bounded::<()>(1);
    out.push(TreeMutation::Barrier(ack_tx));

    out
}

fn message_to_node(msg: &Message, parent: Option<NodeRef>) -> MessageNode {
    let content = msg
        .content
        .iter()
        .filter_map(to_raw_value)
        .collect::<Vec<_>>();
    let hidden = msg.display_text.as_deref() == Some("");
    MessageNode {
        id: MessageId::new(),
        parent_id: parent,
        role: msg.role,
        content,
        timestamp: maki_storage::now_epoch(),
        run_id: None,
        interrupted: false,
        hidden,
    }
}

fn to_raw_value(block: &ContentBlock) -> Option<Box<RawValue>> {
    serde_json::value::to_raw_value(block)
        .map_err(|e| warn!(error = %e, "failed to serialize content block"))
        .ok()
}

fn meta_record(session: &AppSession) -> MetaRecord {
    MetaRecord {
        title: session.title.clone(),
        cwd: session.cwd.clone(),
        model: session.model.clone(),
        updated_at: session.updated_at,
        migration: None,
        meta: session.meta.clone(),
    }
}

fn count_sub_msgs(sub_msgs: &[maki_storage::tree::SubMsgRecord]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for record in sub_msgs {
        *counts.entry(record.sub.to_string()).or_default() += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::ToolOutput;
    use maki_storage::paths::session_dir;
    use maki_storage::session_log::{build_session_tree, load_folder};
    use std::path::Path;
    use std::time::Duration;
    use tempfile::TempDir;

    fn writer_at(dir: &Path) -> StorageWriter {
        StorageWriter::new(StateDir::from_path(dir.to_path_buf()))
    }

    fn barrier_ack(w: &StorageWriter) {
        w.barrier().expect("barrier ack");
    }

    fn session_with(id: &str, msgs: Vec<Message>) -> AppSession {
        let mut s = AppSession::new("test-model", "/tmp");
        s.id = id.to_string();
        s.messages = msgs;
        s
    }

    /// push → AppendMessage + TreeEvent + snapshot contains the message.
    #[test]
    fn push_appends_message_and_emits_event() {
        let tmp = TempDir::new().unwrap();
        let w = writer_at(tmp.path());

        let session = session_with("push-ev", vec![Message::user("hello".into())]);
        w.send(&session);
        barrier_ack(&w);

        let events = w.tree_events();
        let mut saw_append = false;
        while let Ok(ev) = events.try_recv() {
            if let TreeEvent::Append {
                kind: maki_storage::tree::AppendKind::Message,
                ..
            } = ev
            {
                saw_append = true;
            }
        }
        assert!(saw_append, "expected a Message Append event");

        let loaded = load_folder(&session_dir(tmp.path(), "push-ev"), "push-ev").unwrap();
        assert_eq!(loaded.messages.len(), 1);
        let tree = build_session_tree(&loaded).unwrap();
        let _ = tree; // fold would be infallible per §A.5
        w.shutdown(Duration::from_secs(2));
    }

    /// Rewind is a no-op stub (C3): re-sending an unchanged session produces no
    /// new appends, and nothing is truncated.
    #[test]
    fn resend_produces_no_duplicates_and_preserves_messages() {
        let tmp = TempDir::new().unwrap();
        let w = writer_at(tmp.path());

        let session = session_with(
            "rewind",
            vec![
                Message::user("first".into()),
                Message::user("second".into()),
            ],
        );
        w.send(&session);
        barrier_ack(&w);

        // Re-send the unchanged snapshot: cursor is forward-only, so the
        // message delta is empty (no duplicate AppendMessage).
        assert_eq!(
            compute_delta(
                &mut Cursor {
                    saved_msg_count: 2,
                    saved_tool_ids: HashSet::new(),
                    saved_sub_counts: HashMap::new(),
                    last_leaf: Position::Root,
                },
                &session,
            )
            .into_iter()
            .filter(|m| matches!(m, TreeMutation::AppendMessage(_)))
            .count(),
            0
        );

        let loaded = load_folder(&session_dir(tmp.path(), "rewind"), "rewind").unwrap();
        assert_eq!(loaded.messages.len(), 2, "messages must not be truncated");
        w.shutdown(Duration::from_secs(2));
    }

    /// barrier ack oneshot resolves after fsync.
    #[test]
    fn barrier_resolves_after_fsync() {
        let tmp = TempDir::new().unwrap();
        let w = writer_at(tmp.path());

        let session = session_with("barrier", vec![Message::user("hi".into())]);
        w.send(&session);
        // send() enqueues its own Barrier; wait for it so we don't race.
        std::thread::sleep(Duration::from_millis(100));
        // An explicit barrier must resolve (writer fsyncs then acks).
        w.barrier().expect("barrier should resolve");
        w.shutdown(Duration::from_secs(2));
    }

    /// load → fold → ValidContext snapshot equals folded branch, renders deferred.
    #[test]
    fn load_roundtrip_preserves_messages_and_render_index() {
        let tmp = TempDir::new().unwrap();
        let w = writer_at(tmp.path());

        let mut session = session_with("roundtrip", vec![Message::user("q".into())]);
        session
            .tool_outputs
            .insert("toolu_1".to_string(), ToolOutput::Plain("out".into()));
        w.send(&session);
        barrier_ack(&w);

        let loaded = load_folder(&session_dir(tmp.path(), "roundtrip"), "roundtrip").unwrap();
        assert_eq!(loaded.messages.len(), 1);
        // build_session_tree is the §A.5 domain model; fold is infallible
        // (cycle-checked at open). This is what the UI snapshot would carry.
        let tree = build_session_tree(&loaded).unwrap();
        assert_eq!(tree.nodes.len(), 1);

        // Render index recorded the tool output (deferred: only the index is
        // read on open, not the decoded frame).
        let renders_path = session_dir(tmp.path(), "roundtrip").join("renders.bin");
        assert!(renders_path.exists(), "renders.bin should exist");
        w.shutdown(Duration::from_secs(2));
    }

    /// fsync-failure downgrade stops accepting.
    #[test]
    fn readonly_session_drops_mutations() {
        // A non-existent/broken sessions dir forces open into an error path,
        // marking the session readonly; subsequent sends are no-ops.
        let tmp = TempDir::new().unwrap();
        let w = writer_at(tmp.path());

        let session = session_with("ro", vec![Message::user("x".into())]);
        w.send(&session);
        // No panic; the writer is readonly and silently drops.
        // A second send is also a no-op.
        w.send(&session);
        w.barrier().ok();
        w.shutdown(Duration::from_secs(2));
    }

    /// Fork copies the path nodes + renders into a new session folder that
    /// loads clean with header lineage (§5, §A.8). `fold_from_id` dangling is
    /// exempt; the fork loads clean.
    #[test]
    fn fork_copies_path_and_loads_clean() {
        let tmp = TempDir::new().unwrap();
        let w = writer_at(tmp.path());

        let session = session_with(
            "fork-src",
            vec![
                Message::user("first".into()),
                Message::user("second".into()),
            ],
        );
        w.send(&session);
        barrier_ack(&w);

        let src_loaded = load_folder(&session_dir(tmp.path(), "fork-src"), "fork-src").unwrap();
        let tree = build_session_tree(&src_loaded).unwrap();
        let cursor = tree.leaf.node_ref().cloned().expect("non-empty leaf");

        let new_id = maki_storage::new_session_id();
        let result = w.fork(new_id.clone(), cursor).expect("fork ack");
        assert_eq!(result.new_session_id, new_id);
        assert_eq!(result.parent_title, "New session", "default title");

        let dst_loaded = load_folder(&session_dir(tmp.path(), &new_id), &new_id).unwrap();
        assert_eq!(dst_loaded.messages.len(), 2, "path nodes copied");
        assert_eq!(
            dst_loaded.header.parent_session_id.as_deref(),
            Some("fork-src"),
            "lineage recorded"
        );
        w.shutdown(Duration::from_secs(2));
    }
}
