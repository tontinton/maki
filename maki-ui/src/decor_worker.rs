//! Where the chat input's decorations are worked out off the UI thread, and
//! where a finished job wakes the event loop, so a result is drawn on the
//! next frame instead of at the next idle poll.
//!
//! Tree-sitter parses on a thread of its own. Syntax colors of long code run
//! on the highlight thread, which syntect wants to itself to bound its regex
//! cache. Tree-sitter keeps no such cache, and a parse of a big prompt can
//! take tens of milliseconds, which would hold up the transcript's colors
//! and every Lua `pool::run` queued behind it.

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::mem;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::LazyLock;
use std::thread;

use flume::{Receiver, Sender};
use maki_agent::UiWaker;

const PARSE_THREAD: &str = "input-parse";

type Job = Box<dyn FnOnce() + Send>;

static WAKER: LazyLock<(UiWaker, Receiver<()>)> = LazyLock::new(UiWaker::new);

static PARSES: LazyLock<Sender<Job>> = LazyLock::new(|| {
    let (jobs, queued) = flume::unbounded::<Job>();
    thread::Builder::new()
        .name(PARSE_THREAD.into())
        .spawn(move || {
            while let Ok(job) = queued.recv() {
                if catch_unwind(AssertUnwindSafe(job)).is_err() {
                    tracing::error!("input parse panicked");
                }
            }
        })
        .expect("failed to spawn the input parse thread");
    jobs
});

#[cfg(test)]
thread_local! {
    /// Set by [`manual`]: jobs wait here until the test runs them, so it can
    /// look at the frame before a result and after it without timing
    /// anything.
    static MANUAL: RefCell<Option<Vec<Job>>> = const { RefCell::new(None) };
}

/// Ready once a job has finished since the loop last looked.
pub fn wakes() -> &'static Receiver<()> {
    &WAKER.1
}

/// Runs {job} on the highlight thread.
pub fn spawn(job: impl FnOnce() + Send + 'static) {
    send(job, maki_highlight::pool::spawn);
}

pub fn spawn_parse(job: impl FnOnce() + Send + 'static) {
    send(job, |job| {
        let _ = PARSES.send(job);
    });
}

fn send(job: impl FnOnce() + Send + 'static, run: impl FnOnce(Job)) {
    #[cfg(test)]
    let Some(job) = MANUAL.with_borrow_mut(|manual| match manual {
        Some(queued) => {
            queued.push(Box::new(job));
            None
        }
        None => Some(job),
    }) else {
        return;
    };
    run(Box::new(move || {
        job();
        WAKER.0.wake();
    }));
}

/// From now on, jobs spawned from this thread wait for [`run_queued`].
#[cfg(test)]
pub fn manual() {
    MANUAL.set(Some(Vec::new()));
}

/// Runs the jobs waiting, and returns how many there were.
#[cfg(test)]
pub fn run_queued() -> usize {
    let queued =
        MANUAL.with_borrow_mut(|manual| manual.as_mut().map(mem::take).unwrap_or_default());
    let count = queued.len();
    for job in queued {
        job();
    }
    count
}
