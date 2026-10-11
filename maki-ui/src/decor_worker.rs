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
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
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

/// One input's parse waiting for the parse thread. A newer one takes the
/// place of one that has not started, so a burst of rule changes on a big
/// prompt costs one parse, not one each.
#[derive(Default)]
pub struct ParseSlot(Mutex<Option<Job>>);

impl ParseSlot {
    fn put(&self, job: Job) -> Option<Job> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(job)
    }

    fn take(&self) -> Option<Job> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

/// Runs {job} on the parse thread, in place of the one {slot} still waits
/// with.
pub fn spawn_parse(slot: &Arc<ParseSlot>, job: impl FnOnce() + Send + 'static) {
    if slot.put(Box::new(job)).is_some() {
        return;
    }
    let slot = Arc::clone(slot);
    let run = move || {
        if let Some(job) = slot.take() {
            job();
        }
    };
    send(run, |job| {
        let _ = PARSES.send(job);
    });
}

fn send(job: impl FnOnce() + Send + 'static, run: impl FnOnce(Job)) {
    // A job that panicked has finished too, and its input waits to hear it.
    let job: Job = Box::new(move || {
        let finished = catch_unwind(AssertUnwindSafe(job));
        WAKER.0.wake();
        if let Err(panic) = finished {
            resume_unwind(panic);
        }
    });
    #[cfg(test)]
    let Some(job) = MANUAL.with_borrow_mut(|manual| match manual {
        Some(queued) => {
            queued.push(job);
            None
        }
        None => Some(job),
    }) else {
        return;
    };
    run(job);
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

#[cfg(test)]
mod tests {
    use std::panic::catch_unwind;

    use super::{manual, run_queued, spawn, wakes};

    const JOB_PANIC: &str = "the job broke";

    #[test]
    fn a_job_that_panics_still_wakes_the_loop() {
        manual();
        spawn(|| panic!("{JOB_PANIC}"));
        while wakes().try_recv().is_ok() {}

        assert!(catch_unwind(run_queued).is_err());
        assert!(wakes().try_recv().is_ok());
    }
}
