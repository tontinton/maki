//! Thread pool for syntax highlighting. Threads scale up to CPU count and exit after
//! `IDLE_TIMEOUT` (5 s) of inactivity. Jobs carry monotonic u64 IDs so callers can
//! discard stale results.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use tracing::error;

use crate::components::code_view::{self, RenderLimits};
use maki_agent::{ToolInput, ToolOutput};
use ratatui::text::Line;

const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const FALLBACK_MAX_THREADS: usize = 4;

struct RenderJob {
    id: u64,
    tool_input: Option<Arc<ToolInput>>,
    tool_output: Option<Arc<ToolOutput>>,
    limits: RenderLimits,
}

pub struct RenderResult {
    pub id: u64,
    pub lines: Vec<Line<'static>>,
}

static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(0);

struct PoolInner {
    job_rx: flume::Receiver<RenderJob>,
    result_tx: flume::Sender<RenderResult>,
    active_threads: AtomicUsize,
    max_threads: usize,
}

pub struct RenderWorker {
    job_tx: flume::Sender<RenderJob>,
    inner: Arc<PoolInner>,
    result_rx: flume::Receiver<RenderResult>,
}

impl RenderWorker {
    pub fn new() -> Self {
        let (job_tx, job_rx) = flume::unbounded();
        let (result_tx, result_rx) = flume::unbounded();
        let max_threads = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(FALLBACK_MAX_THREADS);

        Self {
            job_tx,
            inner: Arc::new(PoolInner {
                job_rx,
                result_tx,
                active_threads: AtomicUsize::new(0),
                max_threads,
            }),
            result_rx,
        }
    }

    pub fn send(
        &self,
        tool_input: Option<Arc<ToolInput>>,
        tool_output: Option<Arc<ToolOutput>>,
        limits: RenderLimits,
    ) -> u64 {
        let id = NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed);
        let _ = self.job_tx.send(RenderJob {
            id,
            tool_input,
            tool_output,
            limits,
        });
        self.maybe_spawn_thread();
        id
    }

    pub fn try_recv(&self) -> Option<RenderResult> {
        self.result_rx.try_recv().ok()
    }

    fn maybe_spawn_thread(&self) {
        let current = self.inner.active_threads.load(Ordering::Acquire);
        if current >= self.inner.max_threads {
            return;
        }
        if self
            .inner
            .active_threads
            .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let inner = Arc::clone(&self.inner);
        if let Err(e) = thread::Builder::new()
            .name("render".into())
            .spawn(move || worker_loop(&inner))
        {
            self.inner.active_threads.fetch_sub(1, Ordering::AcqRel);
            error!("failed to spawn render thread: {e}");
        }
    }
}

fn worker_loop(inner: &PoolInner) {
    while let Ok(job) = inner.job_rx.recv_timeout(IDLE_TIMEOUT) {
        let content = code_view::render_tool_content(
            job.tool_input.as_deref(),
            job.tool_output.as_deref(),
            true,
            job.limits,
        );
        if inner
            .result_tx
            .send(RenderResult {
                id: job.id,
                lines: content.lines,
            })
            .is_err()
        {
            break;
        }
    }
    inner.active_threads.fetch_sub(1, Ordering::AcqRel);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_worker(active: usize, max: usize) -> RenderWorker {
        let (job_tx, job_rx) = flume::unbounded();
        let (result_tx, result_rx) = flume::unbounded();
        RenderWorker {
            job_tx,
            inner: Arc::new(PoolInner {
                job_rx,
                result_tx,
                active_threads: AtomicUsize::new(active),
                max_threads: max,
            }),
            result_rx,
        }
    }

    #[test]
    fn does_not_spawn_when_at_cap() {
        let worker = make_worker(2, 2);
        worker.maybe_spawn_thread();
        assert_eq!(worker.inner.active_threads.load(Ordering::SeqCst), 2);
    }
}
