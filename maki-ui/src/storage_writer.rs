//! Coalescing write-behind cache with incremental JSONL persistence.
//!
//! Apps post session snapshots keyed by session id; the writer thread drains
//! the newest snapshot of every session per wake and performs O(delta)
//! appends. Deletes run on the same thread, so an append and a delete of the
//! same session can never race: a queued save cannot resurrect deleted files.

use std::collections::{HashMap, HashSet};
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

const SAVE_FAILED_PREFIX: &str = "Session save failed";
const SAVE_RECOVERED: &str = "Session save recovered";

type Pending = Arc<Mutex<HashMap<MakiId, Arc<AppSession>>>>;

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
    pub fn new(dir: StateDir, warn_tx: flume::Sender<String>) -> Self {
        let pending: Pending = Arc::default();
        let writer_pending = Arc::clone(&pending);
        let (ops, ops_rx) = flume::unbounded::<Op>();
        let (done_tx, done_rx) = flume::bounded::<()>(1);

        std::thread::Builder::new()
            .name("storage-writer".into())
            .spawn(move || {
                let mut writer = Writer {
                    dir,
                    warn_tx,
                    logs: HashMap::new(),
                    failing: HashSet::new(),
                };
                while let Ok(op) = ops_rx.recv() {
                    match op {
                        Op::Flush => writer.flush(&writer_pending),
                        Op::Delete { id, done } => {
                            lock(&writer_pending).remove(&id);
                            writer.forget(id);
                            done(AppSession::delete(id, &writer.dir));
                        }
                    }
                }
                writer.flush(&writer_pending);
                let _ = done_tx.send(());
            })
            .expect("failed to spawn storage writer thread");

        Self {
            pending,
            ops,
            done_rx,
        }
    }

    pub fn send(&self, session: Arc<AppSession>) {
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

fn lock(pending: &Pending) -> std::sync::MutexGuard<'_, HashMap<MakiId, Arc<AppSession>>> {
    pending.lock().unwrap_or_else(|e| e.into_inner())
}

fn writer_gone() -> SessionError {
    maki_storage::StorageError::Io(io::Error::other("storage writer unavailable")).into()
}

/// Everything the writer thread owns. It never leaves that thread, so nothing
/// here needs a lock.
struct Writer {
    dir: StateDir,
    warn_tx: flume::Sender<String>,
    /// Only cursors that still describe their file.
    logs: HashMap<MakiId, SessionLog>,
    /// Sessions whose last write failed, so a sick disk warns once instead of
    /// once per frame.
    failing: HashSet<MakiId>,
}

impl Writer {
    fn forget(&mut self, id: MakiId) {
        self.logs.remove(&id);
        self.failing.remove(&id);
    }

    fn flush(&mut self, pending: &Pending) {
        let batch = mem::take(&mut *lock(pending));
        if batch.is_empty() {
            return;
        }
        let sessions_dir = match self.dir.ensure_subdir(SESSIONS_DIR) {
            Ok(d) => d,
            Err(e) => {
                for id in batch.into_keys() {
                    self.report(id, Err(&e));
                }
                return;
            }
        };
        for session in batch.into_values() {
            let result = self.write(&sessions_dir, &session);
            self.report(session.id, result);
        }
    }

    fn write(&mut self, sessions_dir: &Path, session: &AppSession) -> Result<(), SessionError> {
        let mut log = match self.logs.remove(&session.id) {
            Some(log) => log,
            None => open_or_create_log(sessions_dir, session)?,
        };
        // A failed `append` rolls the file back to the last record boundary, so
        // its cursors still fit the file and the log stays usable. Only a failed
        // rewrite leaves us without one, and re-opening costs a full read and
        // reparse, the last thing a failing disk needs.
        let (result, usable_on_error) = match log.append(session) {
            Err(SessionError::LogDiverged { .. }) => (log.compact(sessions_dir, session), false),
            appended => (appended, true),
        };
        if result.is_ok() || usable_on_error {
            self.logs.insert(session.id, log);
        }
        result
    }

    fn report(&mut self, id: MakiId, result: Result<(), impl std::fmt::Display>) {
        match result {
            Ok(()) => {
                if self.failing.remove(&id) {
                    let _ = self.warn_tx.send(SAVE_RECOVERED.to_string());
                }
            }
            Err(e) => {
                warn!(error = %e, %id, "session write failed");
                if self.failing.insert(id) {
                    let _ = self.warn_tx.send(format!("{SAVE_FAILED_PREFIX}: {e}"));
                }
            }
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
    const MODEL: &str = "test-model";
    const CWD: &str = "/tmp/writer";
    const MSG_PREFIX: &str = "msg-";
    const RESUMED_MSG: &str = "resumed";
    const TOOL_ID: &str = "tool-1";
    const TOOL_TEXT: &str = "tool output";
    const TITLE: &str = "renamed after reload";

    fn state_dir() -> (TempDir, StateDir) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        (tmp, dir)
    }

    fn writer(dir: &StateDir) -> (StorageWriter, flume::Receiver<String>) {
        let (warn_tx, warn_rx) = flume::unbounded();
        (StorageWriter::new(dir.clone(), warn_tx), warn_rx)
    }

    fn message_texts(session: &AppSession) -> Vec<String> {
        session
            .messages()
            .iter()
            .map(|m| m.user_text().unwrap_or_default().to_string())
            .collect()
    }

    fn user_message(n: usize) -> maki_providers::Message {
        maki_providers::Message::user(format!("{MSG_PREFIX}{n}"))
    }

    /// A plain file where the sessions dir should be. `create_dir_all` cannot
    /// turn that into a directory, so every flush fails until it is removed.
    fn block_sessions_dir(dir: &StateDir) {
        std::fs::write(dir.path().join(SESSIONS_DIR), "").unwrap();
    }

    /// Snapshots must coalesce per session id, not into one `latest` slot:
    /// two racing sessions used to silently drop one.
    #[test]
    fn shutdown_drains_newest_snapshot_of_every_session() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let a = AppSession::new("test-model", "/tmp/a");
        let mut b = AppSession::new("test-model", "/tmp/b");
        let (a_id, b_id) = (a.id, b.id);
        writer.send(Arc::new(a));
        writer.send(Arc::new(b.clone()));
        b.set_title("renamed".into());
        writer.send(Arc::new(b));
        writer.shutdown(DRAIN_TIMEOUT);

        assert!(AppSession::load(a_id, &dir).is_ok());
        assert_eq!(AppSession::load(b_id, &dir).unwrap().title, "renamed");
    }

    #[test]
    fn delete_discards_pending_snapshot() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let session = AppSession::new("test-model", "/tmp/c");
        let id = session.id;
        writer.send(Arc::new(session));
        let (done_tx, done_rx) = flume::bounded(1);
        writer.delete(id, move |res| {
            let _ = done_tx.send(res);
        });
        writer.shutdown(DRAIN_TIMEOUT);

        assert!(done_rx.recv().unwrap().is_ok());
        assert!(AppSession::load(id, &dir).is_err());
    }

    /// A fresh writer over an existing file has no cursor, so it re-opens the
    /// log and gets cursors for the loaded session, not the live one. The first
    /// append must diverge into a full rewrite instead of landing on stale
    /// offsets.
    #[test]
    fn reopened_log_rewrites_diverged_file_instead_of_appending() {
        let (_tmp, dir) = state_dir();
        let mut session = AppSession::new(MODEL, CWD);
        let id = session.id;
        for i in 0..5 {
            session.push_message(user_message(i));
        }
        let (first, _first_warn_rx) = writer(&dir);
        first.send(Arc::new(session.clone()));
        first.shutdown(DRAIN_TIMEOUT);

        session.truncate_messages(2);
        session.push_message(maki_providers::Message::user(RESUMED_MSG.into()));
        session.insert_tool_output(
            TOOL_ID.into(),
            maki_agent::ToolOutput::Plain(TOOL_TEXT.to_string().into()),
        );
        session.set_title(TITLE.into());

        let (second, second_warn_rx) = writer(&dir);
        second.send(Arc::new(session.clone()));
        second.shutdown(DRAIN_TIMEOUT);

        let loaded = AppSession::load(id, &dir).unwrap();
        assert_eq!(
            message_texts(&loaded),
            [
                format!("{MSG_PREFIX}0"),
                format!("{MSG_PREFIX}1"),
                RESUMED_MSG.to_string()
            ]
        );
        assert_eq!(loaded.title, TITLE);
        match loaded.tool_outputs().get(TOOL_ID) {
            Some(maki_agent::ToolOutput::Plain(out)) => assert_eq!(out.text, TOOL_TEXT),
            other => panic!("tool output lost: {other:?}"),
        }
        assert!(second_warn_rx.is_empty());
    }

    /// A disk that keeps failing warns once, not once per frame, and says so
    /// exactly once when writes start working again.
    #[test]
    fn failing_flush_warns_once_and_reports_recovery() {
        let (_tmp, dir) = state_dir();
        block_sessions_dir(&dir);
        let (writer, warn_rx) = writer(&dir);
        let session = Arc::new(AppSession::new(MODEL, CWD));
        let id = session.id;

        writer.send(Arc::clone(&session));
        let warning = warn_rx.recv_timeout(DRAIN_TIMEOUT).unwrap();
        assert!(warning.starts_with(SAVE_FAILED_PREFIX), "{warning}");

        // The warning above proves the first flush already drained `pending`, so
        // this send queues a second one. The delete runs on the same thread, so
        // waiting on it waits for that flush.
        writer.send(Arc::clone(&session));
        let (done_tx, done_rx) = flume::bounded(1);
        writer.delete(MakiId::generate(), move |res| {
            let _ = done_tx.send(res);
        });
        assert!(done_rx.recv_timeout(DRAIN_TIMEOUT).unwrap().is_err());
        assert!(warn_rx.is_empty(), "second failure warned again");

        std::fs::remove_file(dir.path().join(SESSIONS_DIR)).unwrap();
        writer.send(session);
        let recovered = warn_rx.recv_timeout(DRAIN_TIMEOUT).unwrap();
        assert_eq!(recovered, SAVE_RECOVERED);
        writer.shutdown(DRAIN_TIMEOUT);

        assert!(warn_rx.is_empty());
        assert!(AppSession::load(id, &dir).is_ok());
    }

    /// After a delete the cursor still holds an open handle to the unlinked
    /// file, which still looks unchanged, so an append would write the session
    /// into nothing. Forgetting the cursor makes the next snapshot write a
    /// whole file.
    #[test]
    fn session_recreated_after_delete_is_written_in_full() {
        let (_tmp, dir) = state_dir();
        let (writer, warn_rx) = writer(&dir);
        let mut session = AppSession::new(MODEL, CWD);
        let id = session.id;
        session.push_message(user_message(0));
        writer.send(Arc::new(session.clone()));

        let (done_tx, done_rx) = flume::bounded(1);
        writer.delete(id, move |res| {
            let _ = done_tx.send(res);
        });
        done_rx.recv_timeout(DRAIN_TIMEOUT).unwrap().unwrap();
        assert!(AppSession::load(id, &dir).is_err());

        session.push_message(maki_providers::Message::user(RESUMED_MSG.into()));
        writer.send(Arc::new(session));
        writer.shutdown(DRAIN_TIMEOUT);

        let loaded = AppSession::load(id, &dir).unwrap();
        assert_eq!(
            message_texts(&loaded),
            [format!("{MSG_PREFIX}0"), RESUMED_MSG.to_string()]
        );
        assert!(warn_rx.is_empty());
    }
}
