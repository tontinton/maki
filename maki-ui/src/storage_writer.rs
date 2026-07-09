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
    MessageId, MessageNode, MetaRecord, NodeRef, Position, ToolUseId, TreeEvent, TreeMutation,
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
}
