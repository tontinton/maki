//! Fork copy (§5, §A.8): copy root→cursor into a fresh session folder.
//!
//! Runs on the source session's writer thread. The writer syncs (flushes
//! buffered appends) before dispatching. Stages in a temp dir, fsyncs file
//! contents + dir, then atomically renames into `sessions/<new_id>/` (§14).
//! Snapshot files are copied by the UI layer post-ack (it owns the
//! `SnapshotStore`, §13); the writer owns `log.jsonl`/`renders.bin`/`meta.json`.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::paths::{log_path, meta_path, renders_path};
use crate::renders::{self, RenderStore};
use crate::session_log::LoadedSession;
use crate::tree::{Header, NodeRef, SubMsgRecord, ToolUseId, TreeNode, TreeRecord};
use crate::{StorageError, atomic_write, fsync_dir, now_epoch};

const FORK_TITLE_PREFIX: &str = "(fork of ";
const SNAPSHOTS_SUBDIR: &str = "snapshots";
const OBJECTS_SUBDIR: &str = "objects";
const MANIFEST_EXT: &str = "json";
const SESSION_START_MANIFEST: &str = "session-start";

/// The root→cursor path plus the render ids and subagent transcripts it
/// carries. Leaves/labels on the path are not walked (§A.8); the last copied
/// node becomes the leaf by the leaf rule.
struct ForkPath {
    nodes: Vec<NodeRef>,
    sub_msgs: Vec<SubMsgRecord>,
    render_ids: HashSet<String>,
}

/// A raw content block peeked for tool_use ids without importing `ContentBlock`
/// (crate cycle, §A.10). `tool_use` carries `id`; `tool_result` carries
/// `tool_use_id`.
#[derive(Deserialize)]
struct BlockIds {
    id: Option<String>,
    tool_use_id: Option<String>,
}

/// A serialized `Message` (subagent transcript entry) peeked for the tool_use
/// ids inside its `content`.
#[derive(Deserialize)]
struct TranscriptMsg {
    #[serde(default)]
    content: Vec<BlockIds>,
}

impl ForkPath {
    fn build(loaded: &LoadedSession, from: &NodeRef) -> Self {
        let mut nodes: Vec<NodeRef> = Vec::new();
        let mut cur = Some(from.clone());
        while let Some(nref) = cur {
            let Some(node) = node_in_loaded(loaded, &nref) else {
                break;
            };
            nodes.push(nref.clone());
            cur = node.parent_id();
        }
        nodes.reverse();

        let mut render_ids = HashSet::new();
        for nref in &nodes {
            if let Some(TreeNode::Message(m)) = node_in_loaded(loaded, nref) {
                for raw in &m.content {
                    collect_ids(raw, &mut render_ids);
                }
            }
        }

        let on_path_ids: HashSet<String> = nodes
            .iter()
            .filter_map(|nref| match node_in_loaded(loaded, nref)? {
                TreeNode::Message(m) => Some(message_tool_ids(&m)),
                TreeNode::Summary(_) => None,
            })
            .flatten()
            .collect();

        let sub_msgs: Vec<SubMsgRecord> = loaded
            .sub_msgs
            .iter()
            .filter(|s| on_path_ids.contains(s.sub.as_str()))
            .cloned()
            .collect();

        // Subagent transcripts may reference tool_use ids not on the parent
        // path; their renders must travel with the fork (§5).
        for s in &sub_msgs {
            if let Ok(msg) = serde_json::from_str::<TranscriptMsg>(s.d.get()) {
                for block in &msg.content {
                    if let Some(id) = &block.id {
                        render_ids.insert(id.clone());
                    }
                    if let Some(id) = &block.tool_use_id {
                        render_ids.insert(id.clone());
                    }
                }
            }
        }

        ForkPath {
            nodes,
            sub_msgs,
            render_ids,
        }
    }
}

fn node_in_loaded(loaded: &LoadedSession, nref: &NodeRef) -> Option<TreeNode> {
    match nref {
        NodeRef::Msg(id) => loaded
            .messages
            .iter()
            .find(|m| &m.id == id)
            .map(|m| TreeNode::Message(m.clone())),
        NodeRef::Sum(id) => loaded
            .summaries
            .iter()
            .find(|s| &s.id == id)
            .map(|s| TreeNode::Summary(s.clone())),
    }
}

fn message_tool_ids(m: &crate::tree::MessageNode) -> Vec<String> {
    let mut ids = Vec::new();
    for raw in &m.content {
        if let Ok(block) = serde_json::from_str::<BlockIds>(raw.get()) {
            if let Some(id) = block.id {
                ids.push(id);
            }
            if let Some(id) = block.tool_use_id {
                ids.push(id);
            }
        }
    }
    ids
}

fn collect_ids(raw: &RawValue, out: &mut HashSet<String>) {
    if let Ok(block) = serde_json::from_str::<BlockIds>(raw.get()) {
        if let Some(id) = block.id {
            out.insert(id);
        }
        if let Some(id) = block.tool_use_id {
            out.insert(id);
        }
    }
}

/// Perform the fork copy. `sessions_base` is `<base>/sessions/` (the parent of
/// the source session dir). The caller must sync (flush buffered appends)
/// before invoking (§A.8: `writer.flush()`). Returns the parent title for the
/// `(fork of …)` label.
pub fn fork_to(
    loaded: &LoadedSession,
    renders: &mut RenderStore,
    sessions_base: &Path,
    new_session_id: &str,
    from: &NodeRef,
) -> Result<String, StorageError> {
    let path = ForkPath::build(loaded, from);
    let parent_title = loaded.meta.title.clone();
    let cwd = loaded.header.cwd.clone();
    let old_session_id = loaded.header.session_id.clone();

    let staging = tempfile::tempdir_in(sessions_base)?;
    let new_dir = staging.path().join(new_session_id);
    fs::create_dir_all(&new_dir)?;

    write_log(
        &new_dir,
        loaded,
        &path,
        new_session_id,
        &cwd,
        &old_session_id,
        from,
    )?;
    write_renders(&new_dir, renders, &path.render_ids)?;
    write_meta(&new_dir, &parent_title, &cwd, loaded)?;

    fsync_dir(&new_dir)?;

    let dest = sessions_base.join(new_session_id);
    if dest.exists() {
        return Err(StorageError::Io(std::io::Error::other(
            "fork destination already exists",
        )));
    }
    fs::rename(&new_dir, &dest)?;
    fsync_dir(sessions_base)?;
    Ok(parent_title)
}

fn write_log(
    new_dir: &Path,
    loaded: &LoadedSession,
    path: &ForkPath,
    new_session_id: &str,
    cwd: &str,
    old_session_id: &str,
    from: &NodeRef,
) -> Result<(), StorageError> {
    let mut file = fs::File::create(log_path(new_dir))?;
    let header = Header {
        version: 3,
        session_id: new_session_id.to_string(),
        cwd: cwd.to_string(),
        created_at: now_epoch(),
        parent_session_id: Some(old_session_id.to_string()),
        created_from_node_id: Some(from.clone()),
    };
    write_record(&mut file, &TreeRecord::Header(header))?;

    for nref in &path.nodes {
        match node_in_loaded(loaded, nref) {
            Some(TreeNode::Message(m)) => write_record(&mut file, &TreeRecord::Message(m))?,
            Some(TreeNode::Summary(s)) => write_record(&mut file, &TreeRecord::Summary(s))?,
            None => {}
        }
    }
    for sub in &path.sub_msgs {
        write_record(&mut file, &TreeRecord::SubMsg(sub.clone()))?;
    }
    file.sync_data()?;
    Ok(())
}

fn write_renders(
    new_dir: &Path,
    renders: &mut RenderStore,
    render_ids: &HashSet<String>,
) -> Result<(), StorageError> {
    let mut frames: Vec<(ToolUseId, Vec<u8>)> = Vec::new();
    for id_str in render_ids {
        if let Some(tool_id) = ToolUseId::new(id_str.clone())
            && let Some(frame) = renders.get(&tool_id)
        {
            frames.push((tool_id, frame));
        }
    }
    if frames.is_empty() {
        return Ok(());
    }
    let mut dest = RenderStore::open(&renders_path(new_dir))?;
    let mut compressor = renders::new_compressor()?;
    for (id, frame) in &frames {
        dest.append(id, frame, &mut compressor)?;
    }
    dest.sync_file()?;
    Ok(())
}

fn write_meta(
    new_dir: &Path,
    parent_title: &str,
    cwd: &str,
    loaded: &LoadedSession,
) -> Result<(), StorageError> {
    let title = format!("{FORK_TITLE_PREFIX}{parent_title})");
    let meta = crate::tree::MetaRecord {
        title,
        cwd: cwd.to_string(),
        model: loaded.meta.model.clone(),
        updated_at: now_epoch(),
        migration: None,
        meta: loaded.meta.meta.clone(),
    };
    let json = serde_json::to_vec_pretty(&meta)?;
    atomic_write(&meta_path(new_dir), &json)
}

fn write_record(file: &mut fs::File, record: &TreeRecord) -> Result<(), StorageError> {
    let mut buf = serde_json::to_vec(record)?;
    buf.push(b'\n');
    file.write_all(&buf)?;
    Ok(())
}

/// Copy the cursor snapshot + session-start anchor into the fork so
/// code-restore works (§5, §7). `sessions_base` is `<base>/sessions/`. Resolves
/// `cursor` to its nearest snapshotted ancestor (walking the on-path nodes
/// root→cursor via the tree's parent links). Snapshot files are
/// content-addressed; objects dedup by hash.
pub fn copy_snapshots_for_tree(
    sessions_base: &Path,
    old_session_id: &str,
    new_session_id: &str,
    cursor: &NodeRef,
    tree: &crate::tree::SessionTree,
) -> Result<(), StorageError> {
    let path_nodes = walk_tree_path(tree, cursor);
    copy_snapshots(
        sessions_base,
        old_session_id,
        new_session_id,
        cursor,
        &path_nodes,
    )
}

fn walk_tree_path(tree: &crate::tree::SessionTree, from: &NodeRef) -> Vec<NodeRef> {
    let mut path = Vec::new();
    let mut cur = Some(from.clone());
    while let Some(nref) = cur {
        let Some(node) = tree.nodes.get(&nref) else {
            break;
        };
        path.push(nref);
        cur = node.parent_id();
    }
    path.reverse();
    path
}

fn copy_snapshots(
    sessions_base: &Path,
    old_session_id: &str,
    new_session_id: &str,
    cursor: &NodeRef,
    path_nodes: &[NodeRef],
) -> Result<(), StorageError> {
    let old_snap = snapshots_dir(sessions_base, old_session_id);
    if !old_snap.is_dir() {
        return Ok(());
    }
    let new_snap = sessions_base.join(new_session_id).join(SNAPSHOTS_SUBDIR);
    fs::create_dir_all(&new_snap)?;
    fs::create_dir_all(new_snap.join(OBJECTS_SUBDIR))?;

    let anchor = format!("{SESSION_START_MANIFEST}.{MANIFEST_EXT}");
    if old_snap.join(&anchor).exists() {
        fs::copy(old_snap.join(&anchor), new_snap.join(&anchor))?;
    }

    let cursor_name = nearest_manifest(&old_snap, cursor, path_nodes);
    if let Some(name) = cursor_name {
        let src = old_snap.join(format!("{name}.{MANIFEST_EXT}"));
        let dest = new_snap.join(format!("{name}.{MANIFEST_EXT}"));
        if src.exists() && !dest.exists() {
            fs::copy(src, dest)?;
        }
    }

    copy_referenced_objects(&old_snap, &new_snap)?;
    Ok(())
}

fn nearest_manifest(old_snap: &Path, cursor: &NodeRef, path_nodes: &[NodeRef]) -> Option<String> {
    for nref in path_nodes.iter().rev() {
        let name = nref.to_string();
        if old_snap.join(format!("{name}.{MANIFEST_EXT}")).exists() {
            return Some(name);
        }
    }
    let _ = cursor;
    None
}

fn copy_referenced_objects(old_snap: &Path, new_snap: &Path) -> Result<(), StorageError> {
    let old_objs = old_snap.join(OBJECTS_SUBDIR);
    let new_objs = new_snap.join(OBJECTS_SUBDIR);
    if !old_objs.is_dir() {
        return Ok(());
    }
    let live = live_hashes(new_snap)?;
    for entry in fs::read_dir(&old_objs)? {
        let entry = entry?;
        let name = match entry.file_name().to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        if !live.contains(&name) {
            continue;
        }
        let dest = new_objs.join(&name);
        if dest.exists() {
            continue;
        }
        fs::copy(entry.path(), &dest)?;
    }
    Ok(())
}

fn live_hashes(snap_dir: &Path) -> Result<HashSet<String>, StorageError> {
    let mut live = HashSet::new();
    for entry in fs::read_dir(snap_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some(MANIFEST_EXT) {
            continue;
        }
        let data = match fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        #[derive(Deserialize)]
        struct Entry {
            hash: String,
        }
        let manifest: std::collections::HashMap<String, Entry> = match serde_json::from_slice(&data)
        {
            Ok(m) => m,
            Err(_) => continue,
        };
        for e in manifest.values() {
            live.insert(e.hash.clone());
        }
    }
    Ok(live)
}

fn snapshots_dir(sessions_base: &Path, id: &str) -> PathBuf {
    sessions_base.join(id).join(SNAPSHOTS_SUBDIR)
}

/// The on-path nodes (root→cursor), in order.
pub fn fork_path_nodes(loaded: &LoadedSession, from: &NodeRef) -> Vec<NodeRef> {
    ForkPath::build(loaded, from).nodes
}

/// Render ids the fork must carry: those on the path ∪ those inside copied
/// subagent transcripts (§5).
pub fn fork_render_ids(loaded: &LoadedSession, from: &NodeRef) -> HashSet<String> {
    ForkPath::build(loaded, from).render_ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{MessageId, MessageNode, Role, SummaryId, SummaryKind, SummaryRecord};

    fn make_loaded() -> LoadedSession {
        LoadedSession {
            header: Header::new("src", "/repo", now_epoch()),
            messages: Vec::new(),
            sub_msgs: Vec::new(),
            leaves: Vec::new(),
            summaries: Vec::new(),
            labels: Vec::new(),
            order: Vec::new(),
            meta: crate::tree::MetaRecord {
                title: "parent".into(),
                cwd: "/repo".into(),
                model: "m".into(),
                updated_at: 0,
                migration: None,
                meta: crate::sessions::SessionMeta::default(),
            },
            warnings: Vec::new(),
        }
    }

    fn msg_id(s: &str) -> MessageId {
        serde_json::from_str(&format!("\"msg_{s}\"")).unwrap()
    }

    fn sum_id(s: &str) -> SummaryId {
        serde_json::from_str(&format!("\"sum_{s}\"")).unwrap()
    }

    fn user_node(id: &str, parent: Option<NodeRef>) -> MessageNode {
        MessageNode {
            id: msg_id(id),
            parent_id: parent,
            role: Role::User,
            content: Vec::new(),
            timestamp: 0,
            run_id: None,
            interrupted: false,
            hidden: false,
        }
    }

    fn tool_result_node(id: &str, parent: NodeRef, tool_id: &str) -> MessageNode {
        let raw: Box<RawValue> =
            serde_json::from_str(&format!(r#"{{"tool_use_id":"{tool_id}","content":"r"}}"#))
                .unwrap();
        MessageNode {
            id: msg_id(id),
            parent_id: Some(parent),
            role: Role::User,
            content: vec![raw],
            timestamp: 0,
            run_id: None,
            interrupted: false,
            hidden: false,
        }
    }

    fn assistant_node(id: &str, parent: NodeRef, tool_use_id: &str) -> MessageNode {
        let raw: Box<RawValue> =
            serde_json::from_str(&format!(r#"{{"id":"{tool_use_id}","input":{{}}}}"#)).unwrap();
        MessageNode {
            id: msg_id(id),
            parent_id: Some(parent),
            role: Role::Assistant,
            content: vec![raw],
            timestamp: 0,
            run_id: None,
            interrupted: false,
            hidden: false,
        }
    }

    #[test]
    fn fork_path_walks_cursor_to_root() {
        let mut loaded = make_loaded();
        let n1 = user_node("m1", None);
        let n2 = assistant_node("m2", NodeRef::Msg(n1.id.clone()), "toolu_a");
        let n3 = tool_result_node("m3", NodeRef::Msg(n2.id.clone()), "toolu_a");
        loaded.messages = vec![n1.clone(), n2.clone(), n3.clone()];
        let path = fork_path_nodes(&loaded, &NodeRef::Msg(n3.id.clone()));
        assert_eq!(path.len(), 3);
        assert_eq!(path[0], NodeRef::Msg(n1.id.clone()));
        assert_eq!(path[2], NodeRef::Msg(n3.id.clone()));
    }

    #[test]
    fn render_ids_union_path_and_subagent_transcripts() {
        let mut loaded = make_loaded();
        let n1 = user_node("m1", None);
        let n2 = assistant_node("m2", NodeRef::Msg(n1.id.clone()), "toolu_a");
        loaded.messages = vec![n1, n2.clone()];
        let sub_raw: Box<RawValue> =
            serde_json::from_str(r#"{"content":[{"id":"toolu_sub","input":{}}]}"#).unwrap();
        loaded.sub_msgs = vec![SubMsgRecord {
            sub: ToolUseId::new("toolu_a".into()).unwrap(),
            d: sub_raw,
        }];
        let ids = fork_render_ids(&loaded, &NodeRef::Msg(n2.id.clone()));
        assert!(ids.contains("toolu_a"), "path tool_use id travels");
        assert!(
            ids.contains("toolu_sub"),
            "subagent transcript tool_use id travels"
        );
    }

    #[test]
    fn forks_only_on_path_subagent_transcripts() {
        let mut loaded = make_loaded();
        let n1 = user_node("m1", None);
        loaded.messages = vec![n1.clone()];
        let sub_raw: Box<RawValue> = serde_json::from_str(r#"{"content":[]}"#).unwrap();
        loaded.sub_msgs = vec![SubMsgRecord {
            sub: ToolUseId::new("toolu_unrelated".into()).unwrap(),
            d: sub_raw,
        }];
        let path = ForkPath::build(&loaded, &NodeRef::Msg(n1.id.clone()));
        assert!(path.sub_msgs.is_empty(), "off-path sub_msg not copied");
    }

    /// A branch-summary's `fold_from_id` may dangle after fork (provenance
    /// only, never walked, §5). Forking through it must not fail validation.
    #[test]
    fn branch_summary_fold_from_dangling_exempt() {
        let mut loaded = make_loaded();
        let n1 = user_node("m1", None);
        let summary = SummaryRecord {
            id: sum_id("s1"),
            parent_id: NodeRef::Msg(n1.id.clone()),
            narrative: "abandoned".into(),
            kind: SummaryKind::Branch {
                fold_from_id: NodeRef::Msg(msg_id("dangling")),
            },
            read_files: Vec::new(),
            modified_files: Vec::new(),
        };
        loaded.messages = vec![n1];
        loaded.summaries = vec![summary.clone()];
        let path = fork_path_nodes(&loaded, &NodeRef::Sum(summary.id.clone()));
        assert_eq!(path.len(), 2, "summary plus its parent");
    }
}
