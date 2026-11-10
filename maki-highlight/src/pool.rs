//! Runs syntect on one dedicated thread to bound its regex cache memory.
//!
//! The cache is not syntect's own, it is regex-automata's `Pool<Cache>`. When a
//! thread wants to highlight while another already holds the cache, it gets a
//! fresh set of its own, one per compiled `Regex` in the syntax, and that memory
//! is never given back, not even when the thread exits. It is charged per
//! language too: against a warmed `SyntaxSet` every extra concurrent thread
//! retains about 2.7 MB for `rust` and 1.7 MB for `markdown`, so a session
//! touching several syntaxes pays the bill several times. Nothing caps the
//! total: the 8 stacks the pool keeps per regex only spread contention, each
//! one is an uncapped `Vec`, so every thread that ever contended stays charged.
//! That is how the old render pool, which let workers idle out and respawn
//! every 5s, ended a session at +113 MB after 48 threads had come and gone one
//! at a time.
//!
//! Compiling a syntax's regexes the first time costs a further ~10 MB, but that
//! one is global and paid once, so no amount of single-threading wins it back.

use std::any::Any;
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};
use std::thread::{self, Result as ThreadResult};

use flume::{Receiver, RecvError};
use tracing::error;

const UNKNOWN_PANIC: &str = "<non-string panic payload>";
const THREAD_GONE: &str = "the highlight thread is gone";

type Job = Box<dyn FnOnce() + Send + 'static>;

static JOBS: OnceLock<Sender<Job>> = OnceLock::new();

fn jobs() -> &'static Sender<Job> {
    JOBS.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        thread::Builder::new()
            .name("highlight".into())
            .spawn(move || {
                ON_HIGHLIGHT_THREAD.set(true);
                while let Ok(job) = rx.recv() {
                    if let Err(payload) = catch_unwind(AssertUnwindSafe(job)) {
                        error!(panic = panic_message(&*payload), "highlight job panicked");
                    }
                }
            })
            .expect("failed to spawn the highlight thread");
        tx
    })
}

thread_local! {
    static ON_HIGHLIGHT_THREAD: Cell<bool> = const { Cell::new(false) };
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or(UNKNOWN_PANIC)
}

/// Runs `f` on the highlight thread and waits for its result.
///
/// Nested calls run inline because markdown highlights its own code blocks.
pub fn run<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    if ON_HIGHLIGHT_THREAD.get() {
        return f();
    }
    receive_result(submit(f).recv())
}

/// Waits without occupying a blocking worker. Dropping the future discards
/// queued work; a job already running cannot be interrupted.
pub async fn run_async<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    if ON_HIGHLIGHT_THREAD.get() {
        return f();
    }
    receive_result(submit(f).recv_async().await)
}

fn submit<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Receiver<ThreadResult<T>> {
    let (done_tx, done_rx) = flume::bounded(1);
    spawn(move || {
        if !done_tx.is_disconnected() {
            let _ = done_tx.send(catch_unwind(AssertUnwindSafe(f)));
        }
    });
    done_rx
}

fn receive_result<T>(result: Result<ThreadResult<T>, RecvError>) -> T {
    match result {
        Ok(Ok(value)) => value,
        Ok(Err(payload)) => resume_unwind(payload),
        Err(_) => panic!("{THREAD_GONE}"),
    }
}

pub fn spawn(f: impl FnOnce() + Send + 'static) {
    let _ = jobs().send(Box::new(f));
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::panic::catch_unwind;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::channel;
    use std::task::{Context, Poll, Waker};
    use std::thread;

    use test_case::test_case;

    use super::{panic_message, run, run_async, spawn};

    const PAYLOAD: usize = 41;
    const WAITERS: usize = 8;
    const BOOM: &str = "boom";

    #[test_case(true; "cancelled")]
    #[test_case(false; "awaited")]
    fn dropped_async_waiter_discards_queued_work(cancel: bool) {
        let (release_tx, release_rx) = channel::<()>();
        spawn(move || {
            let _ = release_rx.recv();
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let mut pending = Box::pin(run_async(move || {
            count.fetch_add(1, Ordering::SeqCst);
            PAYLOAD
        }));
        let mut context = Context::from_waker(Waker::noop());
        assert!(pending.as_mut().poll(&mut context).is_pending());
        if cancel {
            drop(pending);
            release_tx.send(()).unwrap();
            run(|| ());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        } else {
            release_tx.send(()).unwrap();
            run(|| ());
            assert_eq!(pending.as_mut().poll(&mut context), Poll::Ready(PAYLOAD));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn runs_job_and_returns_value() {
        assert_eq!(run(|| PAYLOAD + 1), PAYLOAD + 1);
    }

    #[test]
    fn every_job_runs_on_one_thread() {
        let waiters: Vec<_> = (0..WAITERS)
            .map(|_| thread::spawn(|| run(|| thread::current().id())))
            .collect();
        let ids: Vec<_> = waiters
            .into_iter()
            .map(|w| w.join().expect("waiter panicked"))
            .collect();
        assert!(
            ids.windows(2).all(|w| w[0] == w[1]),
            "jobs ran on multiple threads: {ids:?}"
        );
    }

    #[test]
    fn nested_run_does_not_deadlock() {
        assert_eq!(run(|| run(|| PAYLOAD)), PAYLOAD);
    }

    #[test]
    fn many_waiters_all_make_progress() {
        let done = Arc::new(AtomicUsize::new(0));
        let waiters: Vec<_> = (0..WAITERS)
            .map(|_| {
                let done = Arc::clone(&done);
                thread::spawn(move || run(move || done.fetch_add(1, Ordering::AcqRel)))
            })
            .collect();
        for w in waiters {
            w.join().expect("waiter panicked");
        }
        assert_eq!(done.load(Ordering::SeqCst), WAITERS);
    }

    #[test]
    fn survives_a_panicking_job() {
        spawn(|| panic!("{BOOM}"));
        assert_eq!(run(|| PAYLOAD), PAYLOAD);
    }

    #[test]
    fn run_resumes_the_job_panic() {
        let payload = catch_unwind(|| run::<()>(|| panic!("{BOOM}"))).expect_err("job must panic");
        assert_eq!(panic_message(&*payload), BOOM);
    }
}
