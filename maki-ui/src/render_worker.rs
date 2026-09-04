//! Queues tool-content rendering and lets callers discard stale results by ID.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::components::code_view::{self, RenderLimits};
use maki_agent::{ToolInput, ToolOutput};
use ratatui::text::Line;

pub struct RenderResult {
    pub id: u64,
    pub lines: Vec<Line<'static>>,
}

static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(0);

pub struct RenderWorker {
    result_tx: flume::Sender<RenderResult>,
    result_rx: flume::Receiver<RenderResult>,
}

impl RenderWorker {
    pub fn new() -> Self {
        let (result_tx, result_rx) = flume::unbounded();
        Self {
            result_tx,
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
        let result_tx = self.result_tx.clone();
        maki_highlight::pool::spawn(move || {
            let content = code_view::render_tool_content(
                tool_input.as_deref(),
                tool_output.as_deref(),
                true,
                limits,
            );
            let _ = result_tx.send(RenderResult {
                id,
                lines: content.lines,
            });
        });
        id
    }

    pub fn try_recv(&self) -> Option<RenderResult> {
        self.result_rx.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::components::code_view::SectionFlags;

    /// The highlight pool is process-global and shared with every other test in
    /// this binary, so a job can queue behind unrelated work under nextest.
    const RESULT_TIMEOUT: Duration = Duration::from_secs(60);
    const OUTPUT_LIMIT: usize = 64;
    const NO_RESULT: &str = "shared pool never returned the job";
    const UNEXPECTED_RESULT: &str = "worker received a result it did not submit";

    fn recv_result(worker: &RenderWorker) -> RenderResult {
        worker
            .result_rx
            .recv_timeout(RESULT_TIMEOUT)
            .expect(NO_RESULT)
    }

    fn limits() -> RenderLimits {
        RenderLimits::new(SectionFlags::default(), OUTPUT_LIMIT)
    }

    #[test]
    fn empty_jobs_round_trip_through_the_shared_pool() {
        let worker = RenderWorker::new();
        let id = worker.send(None, None, limits());

        assert_eq!(recv_result(&worker).id, id);
    }

    #[test]
    fn results_route_back_to_the_submitting_worker_with_its_own_id() {
        let first = RenderWorker::new();
        let second = RenderWorker::new();

        let first_id = first.send(None, None, limits());
        let second_id = second.send(None, None, limits());
        assert_ne!(first_id, second_id);

        assert_eq!(recv_result(&first).id, first_id);
        assert_eq!(recv_result(&second).id, second_id);

        // Both jobs have completed by now, so a stray result would already be queued.
        assert!(first.try_recv().is_none(), "{UNEXPECTED_RESULT}");
        assert!(second.try_recv().is_none(), "{UNEXPECTED_RESULT}");
    }
}
