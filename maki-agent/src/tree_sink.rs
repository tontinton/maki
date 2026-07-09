//! Headless bridge from in-memory `History` folds to on-disk `TreeWriter`.
//!
//! The TUI path persists via `maki_ui::StorageWriter` (which wraps
//! `TreeWriter` and computes a delta from a full `AppSession` snapshot). The
//! headless/ACP path has no `AppSession`, only the folded active-branch nodes
//! from `History::active_branch_nodes`. This module is the headless
//! equivalent: it opens a session folder, spawns a `TreeWriter` thread, and
//! enqueues `AppendMessage` / `SetMeta` / `Barrier` mutations.
//! Fork/rewind/compact pass through to the writer verbatim. Node-level flags
//! (`interrupted`, `run_id`, `hidden`) are preserved because the headless path
//! appends `MessageNode`s directly rather than re-deriving them from `Message`.
//!
//! Lives in `maki-agent` because `fold_to_messages` needs `Message`
//! (`maki-providers`) and `MessageNode`/`SessionTree` (`maki-storage`);
//! `maki-storage` cannot depend on `maki-providers` (cycle).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use maki_providers::Message;
use maki_storage::StateDir;
use maki_storage::paths::{session_dir, snapshots_dir};
use maki_storage::session_log::{OpenResult, TreeWriter, active_leaf};
use maki_storage::sessions::SESSIONS_DIR;
use maki_storage::tree::{
    ForkResult, MessageId, MessageNode, MetaRecord, NodeRef, Position, TreeEvent, TreeMutation,
};
use tracing::warn;

const WRITER_THREAD_NAME: &str = "tree-sink";
const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

struct Cursor {
    saved_msg_count: usize,
    last_leaf: Position,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            saved_msg_count: 0,
            last_leaf: Position::Root,
        }
    }
}

struct WriterHandle {
    tx: flume::Sender<TreeMutation>,
    events: flume::Receiver<TreeEvent>,
    done_rx: flume::Receiver<()>,
}

pub struct TreeSink {
    dir: StateDir,
    session_id: String,
    handle: Mutex<Option<WriterHandle>>,
    cursor: Mutex<Cursor>,
}

impl TreeSink {
    pub fn open(dir: &StateDir, session_id: &str, _cwd: &str, _model_spec: &str) -> Option<Self> {
        let _ = dir.ensure_subdir(SESSIONS_DIR);
        let base = dir.path();
        let writer = match maki_storage::session_log::open(base, session_id) {
            OpenResult::Writer(w) => w,
            OpenResult::Reader(_) => {
                warn!(session_id, "session locked; persistence disabled");
                return None;
            }
            OpenResult::Unsupported(version) => {
                warn!(session_id, version, "unsupported log version");
                return None;
            }
            OpenResult::Error(e) => {
                warn!(session_id, error = %e, "session open failed");
                return None;
            }
        };

        let saved_msg_count = writer.loaded.messages.len();
        let last_leaf = active_leaf(writer.order(), writer.nodes());

        let (tx, rx) = flume::unbounded::<TreeMutation>();
        let tree_writer = TreeWriter::new(writer, rx);
        let events = tree_writer.events();
        let (done_tx, done_rx) = flume::bounded::<()>(1);
        std::thread::Builder::new()
            .name(WRITER_THREAD_NAME.into())
            .spawn(move || {
                tree_writer.run();
                let _ = done_tx.send(());
            })
            .expect("failed to spawn tree-sink thread");

        Some(Self {
            dir: dir.clone(),
            session_id: session_id.to_string(),
            handle: Mutex::new(Some(WriterHandle {
                tx,
                events,
                done_rx,
            })),
            cursor: Mutex::new(Cursor {
                saved_msg_count,
                last_leaf,
            }),
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn dir(&self) -> &StateDir {
        &self.dir
    }

    pub fn tree_events(&self) -> flume::Receiver<TreeEvent> {
        self.handle
            .lock()
            .unwrap()
            .as_ref()
            .map(|h| h.events.clone())
            .unwrap_or_else(|| flume::unbounded().1)
    }

    pub fn leaf_position(&self) -> Position {
        self.cursor.lock().unwrap().last_leaf.clone()
    }

    pub fn record_turn(
        &self,
        nodes: &[maki_storage::tree::MessageNode],
        model_spec: &str,
        cwd: &str,
        title: &str,
    ) {
        let mutations = {
            let mut cursor = self.cursor.lock().unwrap();
            compute_delta(&mut cursor, nodes, model_spec, cwd, title)
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

    pub fn fork(
        &self,
        new_session_id: String,
        from_node_id: NodeRef,
    ) -> Result<ForkResult, String> {
        let (ack_tx, ack_rx) = flume::bounded::<Result<ForkResult, String>>(1);
        let guard = self.handle.lock().unwrap();
        let Some(handle) = guard.as_ref() else {
            return Err("no writer".into());
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
            return Err("writer disconnected".into());
        }
        drop(guard);
        ack_rx
            .recv()
            .map_err(|_| "writer disconnected".to_string())?
    }

    pub fn rewind(&self, target: Position) -> Result<Position, String> {
        let prev = {
            let mut cursor = self.cursor.lock().unwrap();
            let prev = cursor.last_leaf.clone();
            cursor.last_leaf = target.clone();
            prev
        };
        let guard = self.handle.lock().unwrap();
        let Some(handle) = guard.as_ref() else {
            return Err("no writer".into());
        };
        if handle.tx.send(TreeMutation::Rewind { target }).is_err() {
            return Err("writer disconnected".into());
        }
        drop(guard);

        let (ack_tx, ack_rx) = flume::bounded::<()>(1);
        let guard = self.handle.lock().unwrap();
        let Some(handle) = guard.as_ref() else {
            return Err("no writer".into());
        };
        if handle.tx.send(TreeMutation::Barrier(ack_tx)).is_err() {
            return Err("writer disconnected".into());
        }
        drop(guard);
        ack_rx
            .recv()
            .map_err(|_| "writer disconnected".to_string())?;
        Ok(prev)
    }

    pub fn compact(&self, snapshots_dir_arg: Option<PathBuf>) -> Result<usize, String> {
        let snapshots_dir = snapshots_dir_arg.or_else(|| {
            let session_dir = session_dir(self.dir.path(), &self.session_id);
            Some(snapshots_dir(&session_dir))
        });
        let (ack_tx, ack_rx) = flume::bounded::<Result<usize, String>>(1);
        let guard = self.handle.lock().unwrap();
        let Some(handle) = guard.as_ref() else {
            return Err("no writer".into());
        };
        if handle
            .tx
            .send(TreeMutation::CompactSession {
                snapshots_dir,
                ack: ack_tx,
            })
            .is_err()
        {
            return Err("writer disconnected".into());
        }
        drop(guard);
        ack_rx
            .recv()
            .map_err(|_| "writer disconnected".to_string())?
    }

    pub fn shutdown(self) {
        let handle = self.handle.lock().unwrap().take();
        drop(self.handle);
        drop(self.cursor);
        if let Some(h) = handle {
            drop(h.tx);
            if h.done_rx.recv_timeout(WRITER_DRAIN_TIMEOUT).is_err() {
                warn!("tree-sink did not drain within {WRITER_DRAIN_TIMEOUT:?}");
            }
        }
    }
}

fn compute_delta(
    cursor: &mut Cursor,
    nodes: &[maki_storage::tree::MessageNode],
    model_spec: &str,
    cwd: &str,
    title: &str,
) -> Vec<TreeMutation> {
    let mut out = Vec::new();

    for src in &nodes[cursor.saved_msg_count..] {
        let id = MessageId::new();
        let node = MessageNode {
            id: id.clone(),
            parent_id: cursor.last_leaf.node_ref().cloned(),
            role: src.role,
            content: src.content.clone(),
            timestamp: src.timestamp,
            run_id: src.run_id,
            interrupted: src.interrupted,
            hidden: src.hidden,
        };
        cursor.last_leaf = Position::At(NodeRef::Msg(id));
        cursor.saved_msg_count += 1;
        out.push(TreeMutation::AppendMessage(node));
    }

    out.push(TreeMutation::SetMeta(meta_record(model_spec, cwd, title)));

    let (ack_tx, _ack_rx) = flume::bounded::<()>(1);
    out.push(TreeMutation::Barrier(ack_tx));

    out
}

fn meta_record(model_spec: &str, cwd: &str, title: &str) -> MetaRecord {
    MetaRecord {
        title: title.to_string(),
        cwd: cwd.to_string(),
        model: model_spec.to_string(),
        updated_at: maki_storage::now_epoch(),
        migration: None,
        meta: maki_storage::sessions::SessionMeta::default(),
    }
}

/// Fold a `SessionTree`'s active branch into `Vec<Message>` (§A.4). Shared
/// with `History::fold` but usable without an in-memory `History`, e.g. when
/// loading a tree-format session from disk for the headless/ACP resume path.
pub fn fold_to_messages(tree: &maki_storage::tree::SessionTree) -> Vec<Message> {
    let ctx = crate::History::fold_tree(tree);
    ctx.to_vec()
}

/// Load a session's active-branch messages from the tree format (folder
/// `log.jsonl`). Returns `None` when the session is not in tree format (so
/// callers can fall back to legacy flat-file loading).
pub fn load_messages_from_tree(dir: &StateDir, session_id: &str) -> Option<Vec<Message>> {
    let session_dir = maki_storage::paths::session_dir(dir.path(), session_id);
    if !session_dir.join("log.jsonl").exists() {
        return None;
    }
    let loaded = maki_storage::session_log::load_folder(&session_dir, session_id).ok()?;
    let tree = maki_storage::session_log::build_session_tree(&loaded).ok()?;
    Some(fold_to_messages(&tree))
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_providers::{ContentBlock, Message, Role};
    use maki_storage::paths::session_dir;
    use maki_storage::session_log::{build_session_tree, load_folder};
    use maki_storage::tree::NodeRef;
    use serde_json::value::to_raw_value;
    use tempfile::TempDir;

    const SESSION_ID: &str = "tree-sink-test";
    const CWD: &str = "/project";
    const MODEL_SPEC: &str = "anthropic/claude-test";
    const TITLE: &str = "";

    fn sink_at(dir: &TempDir) -> TreeSink {
        TreeSink::open(
            &StateDir::from_path(dir.path().to_path_buf()),
            SESSION_ID,
            CWD,
            MODEL_SPEC,
        )
        .expect("open sink")
    }

    fn barrier(sink: &TreeSink) {
        sink.barrier().expect("barrier ack");
    }

    fn node_from_message(msg: &Message, parent: Option<NodeRef>) -> MessageNode {
        let content = msg
            .content
            .iter()
            .filter_map(|b| to_raw_value(b).ok())
            .collect::<Vec<_>>();
        MessageNode {
            id: MessageId::new(),
            parent_id: parent,
            role: msg.role,
            content,
            timestamp: maki_storage::now_epoch(),
            run_id: None,
            interrupted: false,
            hidden: msg.display_text.as_deref() == Some(""),
        }
    }

    fn nodes(messages: &[Message]) -> Vec<MessageNode> {
        let mut parent: Option<NodeRef> = None;
        messages
            .iter()
            .map(|m| {
                let node = node_from_message(m, parent.clone());
                parent = Some(NodeRef::Msg(node.id.clone()));
                node
            })
            .collect()
    }

    fn assistant_text(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
            display_text: None,
        }
    }

    #[test]
    fn record_turn_persists_messages_and_reloads() {
        let tmp = TempDir::new().unwrap();
        let sink = sink_at(&tmp);

        let messages = vec![Message::user("hello".into()), assistant_text("hi there")];
        let ns = nodes(&messages);
        sink.record_turn(&ns, MODEL_SPEC, CWD, TITLE);
        barrier(&sink);

        let loaded = load_folder(&session_dir(tmp.path(), SESSION_ID), SESSION_ID).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        let tree = build_session_tree(&loaded).unwrap();
        let folded = fold_to_messages(&tree);
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].role, Role::User);
        assert_eq!(folded[1].role, Role::Assistant);
        sink.shutdown();
    }

    #[test]
    fn record_turn_is_incremental_no_duplicates() {
        let tmp = TempDir::new().unwrap();
        let sink = sink_at(&tmp);

        sink.record_turn(
            &nodes(&[Message::user("first".into())]),
            MODEL_SPEC,
            CWD,
            TITLE,
        );
        barrier(&sink);
        sink.record_turn(
            &nodes(&[
                Message::user("first".into()),
                Message::user("second".into()),
            ]),
            MODEL_SPEC,
            CWD,
            TITLE,
        );
        barrier(&sink);

        let loaded = load_folder(&session_dir(tmp.path(), SESSION_ID), SESSION_ID).unwrap();
        assert_eq!(loaded.messages.len(), 2, "no duplicate appends");
        sink.shutdown();
    }

    #[test]
    fn reopen_resumes_existing_session() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        {
            let sink = TreeSink::open(&dir, SESSION_ID, CWD, MODEL_SPEC).expect("open");
            sink.record_turn(
                &nodes(&[Message::user("first".into())]),
                MODEL_SPEC,
                CWD,
                TITLE,
            );
            sink.barrier().expect("ack");
            sink.shutdown();
        }

        let sink = TreeSink::open(&dir, SESSION_ID, CWD, MODEL_SPEC).expect("reopen");
        sink.record_turn(
            &nodes(&[
                Message::user("first".into()),
                Message::user("second".into()),
            ]),
            MODEL_SPEC,
            CWD,
            TITLE,
        );
        sink.barrier().expect("ack");

        let loaded = load_folder(&session_dir(tmp.path(), SESSION_ID), SESSION_ID).unwrap();
        assert_eq!(loaded.messages.len(), 2, "resumed cursor appended only new");
        sink.shutdown();
    }

    #[test]
    fn fork_creates_new_session_with_copied_nodes() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let sink = TreeSink::open(&dir, SESSION_ID, CWD, MODEL_SPEC).expect("open");
        sink.record_turn(
            &nodes(&[
                Message::user("first".into()),
                Message::user("second".into()),
            ]),
            MODEL_SPEC,
            CWD,
            TITLE,
        );
        barrier(&sink);

        let leaf = sink.leaf_position();
        let leaf_nref = leaf.node_ref().cloned().expect("non-root leaf");

        let new_id = maki_storage::new_session_id();
        let result = sink.fork(new_id.clone(), leaf_nref).expect("fork");
        assert_eq!(result.new_session_id, new_id);
        sink.shutdown();

        let dst = load_folder(&session_dir(tmp.path(), &new_id), &new_id).unwrap();
        assert_eq!(dst.messages.len(), 2, "path nodes copied");
        assert_eq!(
            dst.header.parent_session_id.as_deref(),
            Some(SESSION_ID),
            "lineage recorded"
        );
    }
}
