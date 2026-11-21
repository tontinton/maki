//! Coalescing write-behind cache with incremental JSONL persistence.
//!
//! Apps post session snapshots keyed by session id; the writer task drains
//! the newest snapshot of every session per wake and performs O(delta)
//! appends. Deletes travel through the same per-session slot as saves, so
//! whichever the app asked for last is what reaches disk.
//!
//! Every entry carries the claim it is written under, and the writer holds
//! none of its own. A session stays locked exactly while a tab has it open or
//! a write for it is still queued, so a tab that lets go frees it the moment
//! its last snapshot lands. There is no release message that could overtake
//! that snapshot.
//!
//! Other state files have no session to coalesce under, so they ride the
//! wake itself as one-off jobs and land in the order they were queued.

use std::collections::{HashMap, HashSet};
use std::io;
use std::mem;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use maki_storage::id::MakiId;
use maki_storage::sessions::{SessionClaim, SessionError};
use maki_storage::{StateDir, StorageError, blocking};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::warn;

use crate::AppSession;

const SAVE_FAILED_PREFIX: &str = "Session save failed";
const SAVE_RECOVERED: &str = "Session save recovered";

type Pending = Arc<Mutex<HashMap<MakiId, Entry>>>;

type DeleteCallback = Box<dyn FnOnce(Result<(), SessionError>) + Send>;

type Job = Box<dyn FnOnce(&StateDir) + Send>;

enum Wake {
    Flush,
    Run(Job),
}

/// One slot per session, holding whatever the app asked for last. Deletes
/// used to ride a side channel, where a flush queued before a delete could
/// drain a save enqueued after it, so the delete unlinked a session the app
/// had just saved.
enum Entry {
    Save(Arc<AppSession>, SessionClaim),
    /// The claim when the caller holds one. Acquiring it again here would
    /// conflict with that holder, since a lock is per open file.
    Delete(Option<SessionClaim>, DeleteCallback),
}

pub struct StorageWriter {
    pending: Pending,
    wake: UnboundedSender<Wake>,
    /// Not aborted on drop: a writer let go without [`Self::shutdown`] still
    /// drains what was queued, as long as the runtime lives.
    task: JoinHandle<Vec<MakiId>>,
}

impl StorageWriter {
    /// Spawns the writer task, so it needs a runtime.
    pub fn new(dir: StateDir, warn_tx: UnboundedSender<String>) -> Self {
        let pending: Pending = Arc::default();
        let writer_pending = Arc::clone(&pending);
        let (wake, mut wake_rx) = unbounded_channel::<Wake>();

        let task = tokio::spawn(async move {
            let mut writer = Writer {
                dir,
                warn_tx,
                failing: HashSet::new(),
            };
            while let Some(wake) = wake_rx.recv().await {
                match wake {
                    Wake::Flush => writer = writer.flushed(&writer_pending).await,
                    Wake::Run(job) => {
                        let dir = writer.dir.clone();
                        blocking(move || job(&dir)).await;
                    }
                }
            }
            writer
                .flushed(&writer_pending)
                .await
                .failing
                .into_iter()
                .collect()
        });

        Self {
            pending,
            wake,
            task,
        }
    }

    pub fn send(&self, session: Arc<AppSession>, claim: SessionClaim) {
        self.enqueue(session.id, Entry::Save(session, claim));
    }

    /// Delete a session's files on the writer task; `done` fires there, so
    /// callers never block on disk. Deleting a session that was never written
    /// reports success, and a save enqueued afterwards supersedes the delete.
    /// A caller with the session open passes its claim. Any other session is
    /// claimed here and refused if another process has it.
    pub fn delete(
        &self,
        id: MakiId,
        claim: Option<SessionClaim>,
        done: impl FnOnce(Result<(), SessionError>) + Send + 'static,
    ) {
        self.enqueue(id, Entry::Delete(claim, Box::new(done)));
    }

    /// Runs `job` on the writer task after every job queued before it, and
    /// before [`Self::shutdown`] returns.
    pub fn persist(&self, job: impl FnOnce(&StateDir) + Send + 'static) {
        let _ = self.wake.send(Wake::Run(Box::new(job)));
    }

    fn enqueue(&self, id: MakiId, entry: Entry) {
        lock(&self.pending).insert(id, entry);
        if self.wake.send(Wake::Flush).is_err()
            && let Some(Entry::Delete(_, done)) = lock(&self.pending).remove(&id)
        {
            done(Err(writer_gone()));
        }
    }

    /// Returns the sessions whose last write never reached disk. The caller
    /// reports them, since the screen is still up here.
    #[must_use]
    pub async fn shutdown(self, limit: Duration) -> Vec<MakiId> {
        drop(self.wake);
        match timeout(limit, self.task).await {
            Ok(Ok(failing)) => failing,
            // The rest of the shutdown still has to run, so a writer that
            // died only loses its own report.
            Ok(Err(error)) => {
                warn!(%error, "storage writer stopped before it drained");
                Vec::new()
            }
            Err(_) => {
                warn!("storage writer did not drain within {limit:?}");
                Vec::new()
            }
        }
    }
}

fn lock(pending: &Pending) -> std::sync::MutexGuard<'_, HashMap<MakiId, Entry>> {
    pending.lock().unwrap_or_else(|e| e.into_inner())
}

fn writer_gone() -> SessionError {
    StorageError::Io(io::Error::other("storage writer unavailable")).into()
}

/// Everything the writer task owns. One flush has it at a time, so nothing
/// here needs a lock.
struct Writer {
    dir: StateDir,
    warn_tx: UnboundedSender<String>,
    /// Sessions whose last write failed, so a sick disk warns once instead of
    /// once per frame.
    /// Whatever is still in here when the task ends never reached disk.
    failing: HashSet<MakiId>,
}

impl Writer {
    /// The session files are written by sync code, so a whole flush is one
    /// hop to the blocking pool.
    async fn flushed(mut self, pending: &Pending) -> Self {
        if lock(pending).is_empty() {
            return self;
        }
        let pending = Arc::clone(pending);
        blocking(move || {
            self.flush(&pending);
            self
        })
        .await
    }

    fn flush(&mut self, pending: &Pending) {
        // Bound first: a `for` head temporary lives for the whole loop, so
        // iterating the guard directly would deadlock the re-insert below.
        let batch = mem::take(&mut *lock(pending));
        for (id, entry) in batch {
            match entry {
                Entry::Save(session, claim) => {
                    let result = claim.persist(&*session);
                    if result.is_err() {
                        // `checkpoint` never resends an unchanged revision, so
                        // a dropped snapshot would miss disk for good.
                        // `or_insert` lets a newer op win; the shutdown flush
                        // is the last retry. The claim stays queued with it, so
                        // the session stays locked while a write is owed.
                        lock(pending)
                            .entry(id)
                            .or_insert(Entry::Save(session, claim));
                    }
                    self.report(id, result);
                }
                Entry::Delete(claim, done) => {
                    self.failing.remove(&id);
                    done(self.remove(id, claim));
                }
            }
        }
    }

    fn remove(&self, id: MakiId, claim: Option<SessionClaim>) -> Result<(), SessionError> {
        let claim = match claim {
            Some(claim) => claim,
            None => SessionClaim::acquire(id, &self.dir)?,
        };
        match AppSession::delete(&claim, &self.dir) {
            Err(e) if e.is_not_found() => Ok(()),
            result => result,
        }
    }

    fn report(&mut self, id: MakiId, result: Result<(), SessionError>) {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use maki_storage::model::{persist_model, push_recent, read_model, read_recents};
    use maki_storage::sessions::SESSIONS_DIR;
    use tempfile::TempDir;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::sync::oneshot;

    use super::*;
    use crate::OpenSession;

    const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
    const MODEL: &str = "test-model";
    const FIRST_PICK: &str = "test/first";
    const SECOND_PICK: &str = "test/second";
    const CWD: &str = "/tmp/writer";
    const MSG_PREFIX: &str = "msg-";
    const RESUMED_MSG: &str = "resumed";
    const TOOL_ID: &str = "tool-1";
    const TOOL_TEXT: &str = "tool output";
    const TITLE: &str = "renamed after reload";
    const OWED_WRITE_HOLDS: &str = "a session with a write still owed must stay claimed";
    const LANDED_RELEASES: &str = "a session let go is free once its last snapshot landed";
    const NO_WARNING: &str = "the writer never warned";
    const JOB_PANIC: &str = "job panicked";
    const WRITER_GONE: &str = "the writer task ended without warning";
    const ALL_LANDED: &str = "a drain that wrote everything reports nothing unsaved";
    const REPORTED_UNSAVED: &str = "a transcript that never landed must be named on the way out";

    fn state_dir() -> (TempDir, StateDir) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        (tmp, dir)
    }

    fn writer(dir: &StateDir) -> (StorageWriter, UnboundedReceiver<String>) {
        let (warn_tx, warn_rx) = unbounded_channel();
        (StorageWriter::new(dir.clone(), warn_tx), warn_rx)
    }

    async fn next_warning(warn_rx: &mut UnboundedReceiver<String>) -> String {
        timeout(DRAIN_TIMEOUT, warn_rx.recv())
            .await
            .expect(NO_WARNING)
            .expect(WRITER_GONE)
    }

    async fn drain(writer: StorageWriter) {
        let unsaved = writer.shutdown(DRAIN_TIMEOUT).await;
        assert!(unsaved.is_empty(), "{ALL_LANDED}");
    }

    fn fresh(dir: &StateDir) -> (AppSession, SessionClaim) {
        let OpenSession { session, claim } = OpenSession::fresh(MODEL, CWD, dir);
        (session, claim)
    }

    fn sessions_dir(dir: &StateDir) -> PathBuf {
        dir.path().join(SESSIONS_DIR)
    }

    fn message_texts(session: &AppSession) -> Vec<String> {
        session
            .messages()
            .iter()
            .map(|m| m.user_text().unwrap_or_default().to_string())
            .collect()
    }

    fn msg_text(n: usize) -> String {
        format!("{MSG_PREFIX}{n}")
    }

    fn user_message(n: usize) -> maki_providers::Message {
        maki_providers::Message::user(msg_text(n))
    }

    /// A plain file where the sessions dir should be. `create_dir_all` cannot
    /// turn that into a directory, so every flush fails until it is removed.
    fn block_sessions_dir(dir: &StateDir) {
        std::fs::write(sessions_dir(dir), "").unwrap();
    }

    /// A directory where one session's rewrite puts its temp file, so that
    /// session fails to write while its lock and every other session work.
    fn block_log(dir: &StateDir, id: MakiId) -> PathBuf {
        let blocker = sessions_dir(dir).join(format!("{id}.jsonl.tmp"));
        std::fs::create_dir_all(&blocker).unwrap();
        blocker
    }

    #[tokio::test]
    async fn persist_jobs_land_in_order_before_shutdown_returns() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        for spec in [FIRST_PICK, SECOND_PICK] {
            writer.persist(move |dir| {
                persist_model(dir, spec);
                push_recent(dir, spec);
            });
        }
        drain(writer).await;

        assert_eq!(read_model(&dir).as_deref(), Some(SECOND_PICK));
        assert_eq!(read_recents(&dir), [SECOND_PICK, FIRST_PICK]);
    }

    #[tokio::test]
    async fn a_writer_that_panicked_lets_shutdown_finish() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        writer.delete(MakiId::generate(), None, |_| panic!("{JOB_PANIC}"));

        assert!(writer.shutdown(DRAIN_TIMEOUT).await.is_empty());
    }

    /// Snapshots must coalesce per session id, not into one `latest` slot:
    /// two racing sessions used to silently drop one.
    #[tokio::test]
    async fn shutdown_drains_newest_snapshot_of_every_session() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let (a, a_claim) = fresh(&dir);
        let (mut b, b_claim) = fresh(&dir);
        let (a_id, b_id) = (a.id, b.id);
        writer.send(Arc::new(a), a_claim);
        writer.send(Arc::new(b.clone()), b_claim.clone());
        b.set_title("renamed".into());
        writer.send(Arc::new(b), b_claim);
        drain(writer).await;

        assert!(AppSession::load(a_id, &dir).is_ok());
        assert_eq!(AppSession::load(b_id, &dir).unwrap().title, "renamed");
    }

    #[tokio::test]
    async fn delete_discards_pending_snapshot() {
        let (_tmp, dir) = state_dir();
        let (writer, _warn_rx) = writer(&dir);
        let (session, claim) = fresh(&dir);
        let id = session.id;
        writer.send(Arc::new(session), claim.clone());
        let (done_tx, done_rx) = oneshot::channel();
        writer.delete(id, Some(claim), move |res| {
            let _ = done_tx.send(res);
        });
        drain(writer).await;

        assert!(done_rx.await.unwrap().is_ok());
        assert!(AppSession::load(id, &dir).is_err());
    }

    /// A later run over an existing file holds a new claim and so no cursor.
    /// Its first write must start the file over instead of landing on offsets
    /// that describe the session as the earlier run left it.
    #[tokio::test]
    async fn a_new_claim_rewrites_the_file_instead_of_appending() {
        let (_tmp, dir) = state_dir();
        let (mut session, claim) = fresh(&dir);
        let id = session.id;
        for i in 0..5 {
            session.push_message(user_message(i));
        }
        let (first, _first_warn_rx) = writer(&dir);
        first.send(Arc::new(session.clone()), claim);
        drain(first).await;

        session.truncate_messages(2);
        session.push_message(maki_providers::Message::user(RESUMED_MSG.into()));
        session.insert_tool_output(
            TOOL_ID.into(),
            Arc::new(maki_agent::ToolOutput::Plain(TOOL_TEXT.to_string().into())),
        );
        session.set_title(TITLE.into());

        let (second, second_warn_rx) = writer(&dir);
        let claim = SessionClaim::acquire(id, &dir).expect("the first run let go");
        second.send(Arc::new(session.clone()), claim);
        drain(second).await;

        let loaded = AppSession::load(id, &dir).unwrap();
        assert_eq!(
            message_texts(&loaded),
            [msg_text(0), msg_text(1), RESUMED_MSG.to_string()]
        );
        assert_eq!(loaded.title, TITLE);
        match loaded.tool_outputs().get(TOOL_ID).map(Arc::as_ref) {
            Some(maki_agent::ToolOutput::Plain(out)) => assert_eq!(out.text, TOOL_TEXT),
            other => panic!("tool output lost: {other:?}"),
        }
        assert!(second_warn_rx.is_empty());
    }

    /// A disk that keeps failing warns once, not once per frame, and says so
    /// exactly once when writes start working again.
    #[tokio::test]
    async fn failing_flush_warns_once_and_reports_recovery() {
        let (_tmp, dir) = state_dir();
        block_sessions_dir(&dir);
        let (writer, mut warn_rx) = writer(&dir);
        let (session, claim) = fresh(&dir);
        let session = Arc::new(session);
        let id = session.id;

        writer.send(Arc::clone(&session), claim.clone());
        let warning = next_warning(&mut warn_rx).await;
        assert!(warning.starts_with(SAVE_FAILED_PREFIX), "{warning}");

        // The save is enqueued before the delete, so the flush that runs the
        // delete has already drained it; a repeat failure must stay silent.
        writer.send(Arc::clone(&session), claim.clone());
        let (done_tx, done_rx) = oneshot::channel();
        writer.delete(MakiId::generate(), None, move |res| {
            let _ = done_tx.send(res);
        });
        assert!(done_rx.await.unwrap().is_err());
        assert!(warn_rx.is_empty(), "second failure warned again");

        std::fs::remove_file(sessions_dir(&dir)).unwrap();
        writer.send(session, claim);
        let recovered = next_warning(&mut warn_rx).await;
        assert_eq!(recovered, SAVE_RECOVERED);
        drain(writer).await;

        assert!(warn_rx.is_empty());
        assert!(AppSession::load(id, &dir).is_ok());
    }

    /// A tab that swaps its session out drops its claim right after queueing
    /// the last snapshot. The session must stay locked until that snapshot is
    /// on disk and be free right after, without anyone telling the writer.
    #[tokio::test]
    async fn a_session_let_go_stays_claimed_until_its_last_snapshot_lands() {
        let (_tmp, dir) = state_dir();
        let (writer, mut warn_rx) = writer(&dir);
        let (mut session, claim) = fresh(&dir);
        session.push_message(user_message(0));
        let id = session.id;
        let blocker = block_log(&dir, id);

        writer.send(Arc::new(session), claim);
        let warning = next_warning(&mut warn_rx).await;
        assert!(warning.starts_with(SAVE_FAILED_PREFIX), "{warning}");
        assert!(
            SessionClaim::acquire(id, &dir).is_err(),
            "{OWED_WRITE_HOLDS}"
        );

        std::fs::remove_dir(blocker).unwrap();
        drain(writer).await;

        SessionClaim::acquire(id, &dir).expect(LANDED_RELEASES);
        assert_eq!(
            message_texts(&AppSession::load(id, &dir).unwrap()),
            [msg_text(0)]
        );
    }

    /// The final flush runs after the status bar is gone, so the return value
    /// is the only way a lost transcript gets reported.
    #[tokio::test]
    async fn a_transcript_that_never_landed_is_named_at_shutdown() {
        let (_tmp, dir) = state_dir();
        let (mut session, claim) = fresh(&dir);
        session.push_message(user_message(0));
        let id = session.id;
        let _blocker = block_log(&dir, id);

        let (writer, _warn_rx) = writer(&dir);
        writer.send(Arc::new(session), claim);

        assert_eq!(
            writer.shutdown(DRAIN_TIMEOUT).await,
            vec![id],
            "{REPORTED_UNSAVED}"
        );
    }

    /// A save enqueued after a delete must win: clear a draft and retype it
    /// fast enough, and the queued delete used to unlink the file the retype
    /// had just saved.
    #[tokio::test]
    async fn save_enqueued_after_delete_survives() {
        let (_tmp, dir) = state_dir();
        let (writer, warn_rx) = writer(&dir);
        let (mut session, claim) = fresh(&dir);
        let id = session.id;
        session.push_message(user_message(0));
        writer.send(Arc::new(session.clone()), claim.clone());
        writer.delete(id, Some(claim.clone()), |_| {});
        session.push_message(maki_providers::Message::user(RESUMED_MSG.into()));
        writer.send(Arc::new(session), claim);
        drain(writer).await;

        let loaded = AppSession::load(id, &dir).unwrap();
        assert_eq!(
            message_texts(&loaded),
            [msg_text(0), RESUMED_MSG.to_string()]
        );
        assert!(warn_rx.is_empty());
    }

    /// A failed write stays queued: `checkpoint` never resends an unchanged
    /// revision, so the writer owns the retry, and the shutdown flush is the
    /// last one.
    #[tokio::test]
    async fn failed_write_is_retried_by_a_later_flush() {
        let (_tmp, dir) = state_dir();
        block_sessions_dir(&dir);
        let (writer, mut warn_rx) = writer(&dir);
        let (session, claim) = fresh(&dir);
        let id = session.id;

        writer.send(Arc::new(session), claim);
        let warning = next_warning(&mut warn_rx).await;
        assert!(warning.starts_with(SAVE_FAILED_PREFIX), "{warning}");

        std::fs::remove_file(sessions_dir(&dir)).unwrap();
        drain(writer).await;

        assert!(AppSession::load(id, &dir).is_ok());
        assert_eq!(next_warning(&mut warn_rx).await, SAVE_RECOVERED);
    }

    /// After a delete the cursor still holds an open handle to the unlinked
    /// file, which still looks unchanged, so an append would write the session
    /// into nothing. The delete voids the claim's cursor, so the next snapshot
    /// writes a whole file.
    #[tokio::test]
    async fn session_recreated_after_delete_is_written_in_full() {
        let (_tmp, dir) = state_dir();
        let (writer, warn_rx) = writer(&dir);
        let (mut session, claim) = fresh(&dir);
        let id = session.id;
        session.push_message(user_message(0));
        writer.send(Arc::new(session.clone()), claim.clone());

        let (done_tx, done_rx) = oneshot::channel();
        writer.delete(id, Some(claim.clone()), move |res| {
            let _ = done_tx.send(res);
        });
        done_rx.await.unwrap().unwrap();
        assert!(AppSession::load(id, &dir).is_err());

        session.push_message(maki_providers::Message::user(RESUMED_MSG.into()));
        writer.send(Arc::new(session), claim);
        drain(writer).await;

        let loaded = AppSession::load(id, &dir).unwrap();
        assert_eq!(
            message_texts(&loaded),
            [msg_text(0), RESUMED_MSG.to_string()]
        );
        assert!(warn_rx.is_empty());
    }
}
