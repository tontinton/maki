//! Working-tree snapshot store (§7, §A.9).
//!
//! Content-addressed blob store + per-node manifests + an in-memory stat cache.
//! Independent of the tree conversation model: it captures the git working tree
//! (bash-driven mutations included), not just tool edits. Snapshots participate
//! in retention/GC (§15), not the append-only conversation log.
//!
//! Layout (under `sessions/<id>/snapshots/`):
//! - `objects/<blake3-hex>` — each distinct file content, stored once
//! - `<nodeId>.json` — manifest: `{ relative_path: { hash, mode } }`
//! - `session-start.json` — turn-0 anchor manifest (§7)

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use tracing::warn;

const OBJECTS_DIR: &str = "objects";
const SESSION_START_NAME: &str = "session-start";
const MANIFEST_EXT: &str = "json";
const DEFAULT_SNAPSHOT_CAP: u64 = 512 * 1024 * 1024;

/// The stat cache key: the file's relative path identifies it across turns.
type RelPath = String;

/// `relative_path -> (mtime, size, blake3)`. The first snapshot pays the full
/// hash; later turns are O(changed files) — git's index plays the same trick.
#[derive(Debug, Default, Clone)]
struct StatCache(HashMap<RelPath, CachedStat>);

#[derive(Debug, Clone, Copy)]
struct CachedStat {
    mtime: SystemTime,
    size: u64,
    hash: [u8; HASH_LEN],
}

/// One file entry in a manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

/// `relative_path -> entry`. Written to `<node>.json` (§A.9).
pub type Manifest = HashMap<RelPath, FileEntry>;

const HASH_LEN: usize = 32;

/// Hash bytes with blake3 (the production hasher).
pub fn hash_bytes(bytes: &[u8]) -> [u8; HASH_LEN] {
    blake3::hash(bytes).into()
}

/// Hashing strategy: production is blake3; tests inject a counter to assert
/// the stat cache skips unchanged files. The trait keeps the store generic
/// without exposing the hasher on the public API.
pub trait Hasher: Send + Sync {
    fn hash(&self, bytes: &[u8]) -> [u8; HASH_LEN];
}

/// The default blake3 hasher.
pub struct Blake3Hasher;

impl Hasher for Blake3Hasher {
    fn hash(&self, bytes: &[u8]) -> [u8; HASH_LEN] {
        hash_bytes(bytes)
    }
}

/// The snapshot store (§7). The in-memory stat cache makes incremental
/// snapshots cheap; objects are content-addressed so unchanged files dedup
/// across turns. Holds no session tree state: a caller passes the closing node
/// id and store path at each snapshot/restore.
pub struct SnapshotStore {
    dir: PathBuf,
    stat_cache: Mutex<StatCache>,
    hasher: Box<dyn Hasher>,
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("snapshot for {0} not found")]
    NotFound(String),
}

impl SnapshotStore {
    /// Open (or lazily create) a store at `sessions/<id>/snapshots/`.
    pub fn new(snapshots_dir: PathBuf) -> Self {
        Self::with_hasher(snapshots_dir, Box::new(Blake3Hasher))
    }

    /// Construct with a custom hasher (tests inject a counter).
    fn with_hasher(snapshots_dir: PathBuf, hasher: Box<dyn Hasher>) -> Self {
        Self {
            dir: snapshots_dir,
            stat_cache: Mutex::new(StatCache::default()),
            hasher,
        }
    }

    fn objects_dir(&self) -> PathBuf {
        self.dir.join(OBJECTS_DIR)
    }

    fn manifest_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.{MANIFEST_EXT}"))
    }

    /// Snapshot the working tree at `cwd`, keyed by `node_id` (§A.9). Walks
    /// honoring `.gitignore`; falls back to a full walk outside git. Unchanged
    /// files (per the stat cache) reuse their cached hash — O(changed files)
    /// after the first snapshot.
    pub fn snapshot(&self, cwd: &Path, node_id: &str) -> Result<Manifest, SnapshotError> {
        fs::create_dir_all(&self.dir)?;
        fs::create_dir_all(self.objects_dir())?;
        self.snapshot_into(cwd, node_id)
    }

    /// Snapshot the session-start anchor (§7), taken before the first turn.
    pub fn snapshot_session_start(&self, cwd: &Path) -> Result<Manifest, SnapshotError> {
        self.snapshot(cwd, SESSION_START_NAME)
    }

    fn snapshot_into(&self, cwd: &Path, node_id: &str) -> Result<Manifest, SnapshotError> {
        let mut manifest: Manifest = HashMap::new();
        let mut cache = self.stat_cache.lock().unwrap();
        for file in walk_working_tree(cwd) {
            let mtime = file.meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let size = file.meta.len();
            let reuse = cache
                .0
                .get(&file.rel)
                .is_some_and(|c| c.mtime == mtime && c.size == size);
            let hash = if reuse {
                cache.0.get(&file.rel).unwrap().hash
            } else {
                let bytes = fs::read(&file.abs)?;
                let h = self.hasher.hash(&bytes);
                self.write_object(&h, &bytes)?;
                h
            };
            cache
                .0
                .insert(file.rel.clone(), CachedStat { mtime, size, hash });
            manifest.insert(
                file.rel,
                FileEntry {
                    hash: hex::encode(&hash),
                    mode: mode_of(&file.meta),
                },
            );
        }
        drop(cache);
        self.write_manifest(node_id, &manifest)?;
        self.enforce_cap();
        Ok(manifest)
    }

    fn write_object(&self, hash: &[u8; HASH_LEN], bytes: &[u8]) -> Result<(), SnapshotError> {
        let path = self.objects_dir().join(hex::encode(hash));
        if path.exists() {
            return Ok(());
        }
        fs::write(&path, bytes)?;
        Ok(())
    }

    fn write_manifest(&self, name: &str, manifest: &Manifest) -> Result<(), SnapshotError> {
        maki_storage::atomic_write(&self.manifest_path(name), &serde_json::to_vec(manifest)?)
            .map_err(|e| SnapshotError::Io(io::Error::other(e.to_string())))
    }

    /// Load a manifest by name (a node id or `session-start`).
    pub fn load_manifest(&self, name: &str) -> Result<Manifest, SnapshotError> {
        let path = self.manifest_path(name);
        let data = fs::read(&path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => SnapshotError::NotFound(name.to_string()),
            _ => SnapshotError::Io(e),
        })?;
        Ok(serde_json::from_slice(&data)?)
    }

    fn manifest_exists(&self, name: &str) -> bool {
        self.manifest_path(name).exists()
    }

    /// List manifest names (without extension) by mtime, oldest first (§15 cap).
    fn manifests_oldest_first(&self) -> io::Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if path.extension().and_then(|s| s.to_str()) != Some(MANIFEST_EXT) {
                continue;
            }
            if name == SESSION_START_NAME {
                continue;
            }
            if path.is_dir() {
                continue;
            }
            let mtime = entry
                .metadata()?
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH)
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            out.push((name.to_string(), mtime));
        }
        out.sort_by_key(|(_, t)| *t);
        Ok(out)
    }

    /// Aggregate bytes occupied by objects. Manifests and the dir structure are
    /// overhead against the cap; objects dominate because file content is the
    /// bulk (§15: deduped objects make the cap generous).
    fn total_object_bytes(&self) -> u64 {
        let objects = self.objects_dir();
        fs::read_dir(&objects)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Per-session byte cap (§15): drop oldest manifests first, never the
    /// session-start anchor. Newly-unreferenced objects are GC'd after.
    fn enforce_cap(&self) {
        let cap = DEFAULT_SNAPSHOT_CAP;
        let mut total = self.total_object_bytes();
        if total <= cap {
            return;
        }
        let manifests = match self.manifests_oldest_first() {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "snapshot cap: listing manifests failed");
                return;
            }
        };
        for (name, _) in manifests {
            if total <= cap {
                break;
            }
            if name == SESSION_START_NAME {
                continue;
            }
            let path = self.manifest_path(&name);
            let bytes_before = path.metadata().map(|m| m.len()).unwrap_or(0);
            if fs::remove_file(&path).is_err() {
                continue;
            }
            total = total.saturating_sub(bytes_before);
            self.gc_unreferenced_objects_after_drop();
        }
    }

    /// Drop objects referenced by no live manifest (§A.9 GC).
    fn gc_unreferenced_objects_after_drop(&self) {
        let live = match self.live_hashes() {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "snapshot GC: live-hash scan failed");
                return;
            }
        };
        let objects = self.objects_dir();
        let rd = match fs::read_dir(&objects) {
            Ok(rd) => rd,
            Err(_) => return,
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !live.contains(name) && fs::remove_file(&path).is_err() {
                // Best-effort; leave it for a future pass.
            }
        }
    }

    fn live_hashes(&self) -> io::Result<std::collections::HashSet<String>> {
        let mut live = std::collections::HashSet::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some(MANIFEST_EXT) {
                continue;
            }
            let data = match fs::read(&path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let manifest: Manifest = match serde_json::from_slice(&data) {
                Ok(m) => m,
                Err(_) => continue,
            };
            for entry in manifest.values() {
                live.insert(entry.hash.clone());
            }
        }
        Ok(live)
    }

    /// Restore the working tree to the target snapshot (§7, §A.9). The current
    /// tree is snapshotted first (the restore is undoable); the touchable set
    /// is the union of the two manifests' paths — files in the target are
    /// written; files in the pre-restore snapshot but not the target are
    /// deleted (created since); files in neither are never touched.
    ///
    /// `candidates` is the target node id followed by its ancestors root-ward
    /// (the caller supplies ancestry — the store is tree-independent, §A.9). The
    /// first candidate with a manifest wins; if none, the session-start anchor
    /// is used (so restore-to-root = "undo everything this session did", §7).
    pub fn restore(&self, cwd: &Path, candidates: &[&str]) -> Result<Manifest, SnapshotError> {
        let undoing_restore = candidates.contains(&UNDO_MARKER);
        let pre = if undoing_restore {
            self.load_manifest(UNDO_MARKER).unwrap_or_default()
        } else {
            self.snapshot(cwd, UNDO_MARKER)?
        };
        let target = self.resolve_manifest(candidates)?;
        self.materialize(cwd, &target)?;
        self.delete_created_since(cwd, &pre, &target)?;
        Ok(target)
    }

    /// Resolve the first candidate with a manifest, bottoming out at the
    /// session-start anchor (§7). The caller passes `[target, ancestors...]`.
    pub fn resolve_manifest(&self, candidates: &[&str]) -> Result<Manifest, SnapshotError> {
        for name in candidates {
            if self.manifest_exists(name) {
                return self.load_manifest(name);
            }
        }
        if self.manifest_exists(SESSION_START_NAME) {
            return self.load_manifest(SESSION_START_NAME);
        }
        let target = candidates.first().copied().unwrap_or("");
        Err(SnapshotError::NotFound(target.to_string()))
    }

    fn materialize(&self, cwd: &Path, manifest: &Manifest) -> Result<(), SnapshotError> {
        for (rel, entry) in manifest {
            let dest = cwd.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let object = self.objects_dir().join(&entry.hash);
            if object.exists() {
                fs::copy(&object, &dest)?;
                if let Some(mode) = entry.mode {
                    set_mode(&dest, mode);
                }
            }
        }
        Ok(())
    }

    /// Delete files present in `pre` but not `target` (created since the target
    /// snapshot). Files in neither manifest are never touched (§A.9).
    fn delete_created_since(
        &self,
        cwd: &Path,
        pre: &Manifest,
        target: &Manifest,
    ) -> Result<(), SnapshotError> {
        for rel in pre.keys() {
            if target.contains_key(rel) {
                continue;
            }
            let path = cwd.join(rel);
            if path.exists() {
                let _ = fs::remove_file(&path);
            }
        }
        Ok(())
    }

    /// `true` if the session-start anchor exists (first turn has happened).
    pub fn has_session_start(&self) -> bool {
        self.manifest_exists(SESSION_START_NAME)
    }
}

/// The pre-restore `restore` snapshot is keyed by this marker; it is itself a
/// real manifest so restore-undo resolves it, then walks back to the target.
const UNDO_MARKER: &str = "pre-restore";

/// A walked file: relative path, absolute path, metadata.
struct WalkedFile {
    rel: RelPath,
    abs: PathBuf,
    meta: fs::Metadata,
}

/// Walk `cwd` honoring `.gitignore` (§A.9). Falls back to a full walk outside
/// git (the `ignore` crate degrades gracefully when no `.git` is present).
fn walk_working_tree(cwd: &Path) -> Vec<WalkedFile> {
    let cwd_str = match cwd.to_str() {
        Some(s) => s,
        None => return Vec::new(),
    };
    let mut builder = WalkBuilder::new(cwd_str);
    builder
        .hidden(false)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);
    let walker = builder.build();
    let mut out = Vec::new();
    for result in walker {
        let Ok(entry) = result else { continue };
        let file_type = match entry.file_type() {
            Some(ft) if ft.is_file() => ft,
            _ => continue,
        };
        let _ = file_type;
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let rel = match path.strip_prefix(cwd) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel_str = match rel.to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        out.push(WalkedFile {
            rel: rel_str,
            abs: path.to_path_buf(),
            meta,
        });
    }
    out
}

fn mode_of(meta: &fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(meta.permissions().mode())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

/// Hex encoding without an extra dep (blake3 is 32 bytes).
mod hex {
    const HEX: &[u8] = b"0123456789abcdef";
    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    /// A counting hasher so the stat-cache test can assert unchanged files are
    /// not re-hashed. Implements the `Hasher` trait so it wires into the store.
    struct CountingHasher {
        calls: AtomicUsize,
    }
    impl CountingHasher {
        const fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
        fn count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    impl Hasher for CountingHasher {
        fn hash(&self, bytes: &[u8]) -> [u8; HASH_LEN] {
            self.calls.fetch_add(1, Ordering::SeqCst);
            hash_bytes(bytes)
        }
    }

    fn write_file(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn store_for(tmp: &TempDir) -> (SnapshotStore, PathBuf) {
        let snapshots = tmp.path().join("snapshots");
        let store = SnapshotStore::new(snapshots.clone());
        (store, snapshots)
    }

    fn store_with_hasher(tmp: &TempDir, hasher: Box<dyn Hasher>) -> (SnapshotStore, PathBuf) {
        let snapshots = tmp.path().join("snapshots");
        let store = SnapshotStore::with_hasher(snapshots.clone(), hasher);
        (store, snapshots)
    }

    #[test]
    fn snapshot_mutate_restore_round_trip() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        write_file(&cwd, "a.txt", "alpha");
        write_file(&cwd, "b.txt", "beta");

        let (store, _) = store_for(&tmp);
        let node1 = "msg_node1";
        let manifest1 = store.snapshot(&cwd, node1).unwrap();
        assert_eq!(manifest1.len(), 2);

        write_file(&cwd, "a.txt", "ALPHA2");
        write_file(&cwd, "c.txt", "gamma");

        let restored = store.restore(&cwd, &[node1]).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(fs::read_to_string(cwd.join("a.txt")).unwrap(), "alpha");
        assert_eq!(fs::read_to_string(cwd.join("b.txt")).unwrap(), "beta");
        assert!(
            !cwd.join("c.txt").exists(),
            "created-since file must be deleted"
        );
    }

    #[test]
    fn restore_leaves_untracked_files_untouched() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        write_file(&cwd, "tracked.txt", "v1");
        write_file(&cwd, "untracked.txt", "leave me");

        let (store, _) = store_for(&tmp);
        let node = "msg_untracked";
        let _ = store.snapshot(&cwd, node).unwrap();

        write_file(&cwd, "tracked.txt", "v2");
        store.restore(&cwd, &[node]).unwrap();
        assert_eq!(fs::read_to_string(cwd.join("tracked.txt")).unwrap(), "v1");
        assert_eq!(
            fs::read_to_string(cwd.join("untracked.txt")).unwrap(),
            "leave me",
            "files untracked by both manifests are never touched"
        );
    }

    #[test]
    fn restore_undo_restores_pre_state() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        write_file(&cwd, "a.txt", "one");

        let (store, _) = store_for(&tmp);
        let node = "msg_undo";
        let _ = store.snapshot(&cwd, node).unwrap();
        write_file(&cwd, "a.txt", "two");

        store.restore(&cwd, &[node]).unwrap();
        assert_eq!(fs::read_to_string(cwd.join("a.txt")).unwrap(), "one");

        store.restore(&cwd, &[UNDO_MARKER]).unwrap();
        assert_eq!(
            fs::read_to_string(cwd.join("a.txt")).unwrap(),
            "two",
            "pre-restore snapshot enables undoing the restore"
        );
    }

    #[test]
    fn restore_to_root_uses_session_start_anchor() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        write_file(&cwd, "a.txt", "start");

        let (store, _) = store_for(&tmp);
        store.snapshot_session_start(&cwd).unwrap();

        write_file(&cwd, "a.txt", "changed");
        write_file(&cwd, "b.txt", "new");

        let restored = store.restore(&cwd, &["msg_no_manifest"]).unwrap();
        assert!(restored.contains_key("a.txt"));
        assert!(!restored.contains_key("b.txt"));
        assert_eq!(fs::read_to_string(cwd.join("a.txt")).unwrap(), "start");
        assert!(!cwd.join("b.txt").exists());
    }

    #[test]
    fn snapshotless_node_resolves_to_nearest_ancestor() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        write_file(&cwd, "a.txt", "v1");

        let (store, _) = store_for(&tmp);
        store.snapshot_session_start(&cwd).unwrap();
        write_file(&cwd, "a.txt", "v2");
        store.snapshot(&cwd, "msg_ancestor").unwrap();
        write_file(&cwd, "a.txt", "v3");

        // The caller passes the snapshotless node plus its ancestors root-ward
        // (the store is tree-independent — ancestry comes from the caller, §A.9).
        let restored = store
            .restore(&cwd, &["msg_child_no_manifest", "msg_ancestor"])
            .unwrap();
        assert_eq!(fs::read_to_string(cwd.join("a.txt")).unwrap(), "v2");
        assert!(restored.contains_key("a.txt"));
    }

    #[test]
    fn cancelled_run_gets_a_snapshot() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        write_file(&cwd, "a.txt", "before");

        let (store, _) = store_for(&tmp);
        let node = "msg_cancelled";
        let manifest = store.snapshot(&cwd, node).unwrap();
        assert!(!manifest.is_empty());
        assert!(store.manifest_exists(node));
    }

    #[test]
    fn stat_cache_skips_unchanged_files() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        write_file(&cwd, "a.txt", "alpha");
        write_file(&cwd, "b.txt", "beta");
        write_file(&cwd, "c.txt", "gamma");

        let counter = Box::leak(Box::new(CountingHasher::new()));
        let (store, _) = store_with_hasher(&tmp, Box::new(StatCounter(counter)));

        let _ = store.snapshot(&cwd, "msg_first").unwrap();
        let after_first = counter.count();
        assert_eq!(after_first, 3, "first snapshot hashes all files once each");

        // No file changed: the second snapshot reuses every cached hash.
        let _ = store.snapshot(&cwd, "msg_second").unwrap();
        assert_eq!(
            counter.count(),
            after_first,
            "stat cache must skip unchanged files"
        );

        // One file changed: only it is re-hashed.
        write_file(&cwd, "a.txt", "ALPHA2");
        let _ = store.snapshot(&cwd, "msg_third").unwrap();
        assert_eq!(
            counter.count(),
            after_first + 1,
            "only the changed file is re-hashed"
        );
    }

    struct StatCounter(&'static CountingHasher);
    impl Hasher for StatCounter {
        fn hash(&self, bytes: &[u8]) -> [u8; HASH_LEN] {
            self.0.hash(bytes)
        }
    }

    #[test]
    fn cap_drops_oldest_but_never_anchor() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        write_file(&cwd, "big.txt", &"x".repeat(1000));

        let snapshots = tmp.path().join("snapshots");
        fs::create_dir_all(&snapshots).unwrap();
        let store = SnapshotStore::new(snapshots);

        store.snapshot_session_start(&cwd).unwrap();
        for i in 0..5 {
            write_file(&cwd, &format!("f{i}.txt"), &"y".repeat(1000));
            store.snapshot(&cwd, &format!("msg_cap_{i}")).unwrap();
        }

        assert!(store.manifest_exists(SESSION_START_NAME), "anchor survives");
        assert!(store.total_object_bytes() > 0);
    }

    #[test]
    fn session_start_manifest_name() {
        assert_eq!(SESSION_START_NAME, "session-start");
    }

    #[test]
    fn dedup_shares_objects_across_turns() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        write_file(&cwd, "same.txt", "identical content");

        let (store, _) = store_for(&tmp);
        store.snapshot(&cwd, "msg_a").unwrap();
        let objects_after_a = count_objects(&store);
        store.snapshot(&cwd, "msg_b").unwrap();
        let objects_after_b = count_objects(&store);
        assert_eq!(
            objects_after_a, objects_after_b,
            "unchanged content dedups to one object"
        );
    }

    fn count_objects(store: &SnapshotStore) -> usize {
        fs::read_dir(store.objects_dir())
            .map(|rd| rd.count())
            .unwrap_or(0)
    }
}
