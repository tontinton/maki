//! Coalescing write-behind cache with incremental JSONL persistence.
//!
//! Apps post session snapshots keyed by session id; the writer thread drains
//! the newest snapshot of every session per wake and performs O(delta)
//! appends. Deletes run on the same thread, so an append and a delete of the
//! same session can never race: a queued save cannot resurrect deleted files.

use std::collections::HashMap;
use std::io;
use std::mem;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use maki_storage::StateDir;
use maki_storage::id::MakiId;
use maki_storage::sessions::{SESSIONS_DIR, SessionError, SessionLog};
use tracing::warn;

use crate::AppSession;

type Pending = Arc<Mutex<HashMap<MakiId, Box<AppSession>>>>;

type DeleteCallback = Box<dyn FnOnce(Result<(), SessionError>) + Send>;

enum Op {
    Flush,
    Delete { id: MakiId, done: DeleteCallback },
}

pub struct StorageWriter {
    pending: Pending,
    ops: flume::Sender<Op>,
    done_rx: flume::Receiver<()>,
}

impl StorageWriter {
    pub fn new(dir: StateDir) -> Self {
        let pending: Pending = Arc::default();
        let writer_pending = Arc::clone(&pending);
        let (ops, ops_rx) = flume::unbounded::<Op>();
        let (done_tx, done_rx) = flume::bounded::<()>(1);

        std::thread::Builder::new()
            .name("storage-writer".into())
            .spawn(move || {
                let mut logs: HashMap<MakiId, SessionLog> = HashMap::new();
                while let Ok(op) = ops_rx.recv() {
                    match op {
                        Op::Flush => flush(&writer_pending, &mut logs, &dir),
                        Op::Delete { id, done } => {
                            lock(&writer_pending).remove(&id);
                            logs.remove(&id);
                            done(AppSession::delete(id, &dir));
                        }
                    }
                }
                flush(&writer_pending, &mut logs, &dir);
                let _ = done_tx.send(());
            })
            .expect("failed to spawn storage writer thread");

        Self {
            pending,
            ops,
            done_rx,
        }
    }

    pub fn send(&self, session: Box<AppSession>) {
        let mut pending = lock(&self.pending);
        let was_empty = pending.is_empty();
        pending.insert(session.id, session);
        drop(pending);
        if was_empty {
            let _ = self.ops.send(Op::Flush);
        }
    }

    /// Delete a session's files on the writer thread, discarding any pending
    /// snapshot first. Runs after already-queued flushes; `done` fires on the
    /// writer thread, so callers never block on disk.
    pub fn delete(&self, id: MakiId, done: impl FnOnce(Result<(), SessionError>) + Send + 'static) {
        let op = Op::Delete {
            id,
            done: Box::new(done),
        };
        if let Err(flume::SendError(Op::Delete { done, .. })) = self.ops.send(op) {
            done(Err(writer_gone()));
        }
    }

    pub fn shutdown(self, timeout: Duration) {
        drop(self.ops);
        if self.done_rx.recv_timeout(timeout).is_err() {
            warn!("storage writer did not drain within {timeout:?}");
        }
    }
}

fn lock(pending: &Pending) -> std::sync::MutexGuard<'_, HashMap<MakiId, Box<AppSession>>> {
    pending.lock().unwrap_or_else(|e| e.into_inner())
}

fn writer_gone() -> SessionError {
    maki_storage::StorageError::Io(io::Error::other("storage writer unavailable")).into()
}

fn flush(pending: &Pending, logs: &mut HashMap<MakiId, SessionLog>, dir: &StateDir) {
    let batch = mem::take(&mut *lock(pending));
    if batch.is_empty() {
        return;
    }
    let sessions_dir = match dir.ensure_subdir(SESSIONS_DIR) {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "failed to ensure sessions dir");
            return;
        }
    };
    for session in batch.into_values() {
        write_session(&sessions_dir, logs, &session);
    }
}

fn write_session(
    sessions_dir: &Path,
    logs: &mut HashMap<MakiId, SessionLog>,
    session: &AppSession,
) {
    if let Some(log) = logs.get_mut(&session.id) {
        if !append_or_compact(log, sessions_dir, session) {
            logs.remove(&session.id);
        }
        return;
    }
    let mut log = match open_or_create_log(sessions_dir, session) {
        Ok(l) => l,
        Err(e) => {
            warn!(error = %e, id = %session.id, "session log open failed");
            return;
        }
    };
    if append_or_compact(&mut log, sessions_dir, session) {
        logs.insert(session.id, log);
    }
}

/// False means the log's cursors are unusable and it must not stay cached.
fn append_or_compact(log: &mut SessionLog, sessions_dir: &Path, session: &AppSession) -> bool {
    match log.append(session) {
        Ok(()) => true,
        Err(SessionError::CursorAhead { .. }) => match log.compact(sessions_dir, session) {
            Ok(()) => true,
            Err(e) => {
                warn!(error = %e, id = %session.id, "compact fallback failed");
                false
            }
        },
        Err(e) => {
            warn!(error = %e, id = %session.id, "append failed");
            true
        }
    }
}

fn open_or_create_log(
    sessions_dir: &Path,
    session: &AppSession,
) -> Result<SessionLog, maki_storage::sessions::SessionError> {
    let jsonl_path = sessions_dir.join(format!("{}.jsonl", session.id));
    if jsonl_path.exists() {
        let id = session.id;
        let (_loaded, log) = SessionLog::open::<
            maki_providers::Message,
            maki_providers::TokenUsage,
            maki_agent::ToolOutput,
        >(sessions_dir, id)?;
        Ok(log)
    } else {
        AppSession::migrate_to_jsonl(sessions_dir, session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

    fn state_dir() -> (TempDir, StateDir) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        (tmp, dir)
    }

    /// Snapshots must coalesce per session id, not into one `latest` slot:
    /// two racing sessions used to silently drop one.
    #[test]
    fn shutdown_drains_newest_snapshot_of_every_session() {
        let (_tmp, dir) = state_dir();
        let writer = StorageWriter::new(dir.clone());
        let a = AppSession::new("test-model", "/tmp/a");
        let mut b = AppSession::new("test-model", "/tmp/b");
        let (a_id, b_id) = (a.id, b.id);
        writer.send(Box::new(a));
        writer.send(Box::new(b.clone()));
        b.title = "renamed".into();
        writer.send(Box::new(b));
        writer.shutdown(DRAIN_TIMEOUT);

        assert!(AppSession::load(a_id, &dir).is_ok());
        assert_eq!(AppSession::load(b_id, &dir).unwrap().title, "renamed");
    }

    #[test]
    fn delete_discards_pending_snapshot() {
        let (_tmp, dir) = state_dir();
        let writer = StorageWriter::new(dir.clone());
        let session = AppSession::new("test-model", "/tmp/c");
        let id = session.id;
        writer.send(Box::new(session));
        let (done_tx, done_rx) = flume::bounded(1);
        writer.delete(id, move |res| {
            let _ = done_tx.send(res);
        });
        writer.shutdown(DRAIN_TIMEOUT);

        assert!(done_rx.recv().unwrap().is_ok());
        assert!(AppSession::load(id, &dir).is_err());
    }
}
