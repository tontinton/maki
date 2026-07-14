//! Work handed from the UI to the agent loop, and the run phase it drives.
//!
//! [`Activity`] is the single source of truth for "what is the agent doing":
//! the pending queue and the current run phase live behind one mutex, co-owned
//! by the UI (which enqueues, removes, and cancels) and the agent loop (which
//! runs the work). Everything the UI shows — spinner, transient error, queue
//! panel — is a pure projection of this value (`is_busy`, `status_view`,
//! `panel_entries`); nothing is mirrored in the UI or hand-poked.
//!
//! Shutdown rides on `Drop`: when the last [`QueueSender`] goes away, flume
//! closes the notify channel, and the receiver's `recv_notify` wakes with an
//! `Err` and the agent loop falls out of its main loop on its own.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use maki_agent::{AgentInput, ExtractedCommand, ImageSource, InterruptSource};

use crate::components::input::Submission;
use crate::components::queue_panel::QueueEntry;
use crate::components::{ERROR_DISPLAY, StatusView};
use crate::theme;

const COMPACT_LABEL: &str = "/compact";

type Shared = Arc<Mutex<Activity>>;

pub(crate) struct QueuedMessage {
    pub(crate) text: String,
    pub(crate) images: Vec<ImageSource>,
}

impl From<Submission> for QueuedMessage {
    fn from(sub: Submission) -> Self {
        Self {
            text: sub.text,
            images: sub.images,
        }
    }
}

pub(crate) enum QueueItem {
    Message {
        text: String,
        image_count: usize,
        input: AgentInput,
        run_id: u64,
        /// `true` when the UI already drew the bubble (immediate dispatch).
        /// The agent then skips `QueueItemConsumed` so we don't draw it twice.
        /// `false` when the user typed while the agent was busy: the UI waits
        /// for `QueueItemConsumed` before drawing.
        displayed: bool,
    },
    Compact {
        run_id: u64,
    },
}

impl QueueItem {
    pub(crate) fn run_id(&self) -> u64 {
        match self {
            Self::Message { run_id, .. } | Self::Compact { run_id } => *run_id,
        }
    }

    fn as_queue_entry(&self) -> QueueEntry<'static> {
        match self {
            Self::Message { text, .. } => QueueEntry {
                text: Cow::Owned(text.clone()),
                color: theme::current().foreground,
            },
            Self::Compact { .. } => QueueEntry {
                text: Cow::Borrowed(COMPACT_LABEL),
                color: theme::current()
                    .queue
                    .fg
                    .unwrap_or(theme::current().foreground),
            },
        }
    }

    fn into_extracted_command(self) -> ExtractedCommand {
        match self {
            Self::Message { input, run_id, .. } => ExtractedCommand::Interrupt(input, run_id),
            Self::Compact { run_id } => ExtractedCommand::Compact(run_id),
        }
    }

    /// Immediate-dispatch messages already sit in the chat, so hiding them
    /// here stops the panel from reserving a row the agent is about to free,
    /// which used to make the bubble hop up by one frame.
    fn visible_in_panel(&self) -> bool {
        match self {
            Self::Message { displayed, .. } => !displayed,
            Self::Compact { .. } => true,
        }
    }
}

/// The single source of truth for run status. Co-owned by the UI and the agent
/// loop under one mutex. `phase` being an enum makes "running *and* errored"
/// unrepresentable; "queued but idle" is impossible because the same `pending`
/// list feeds both the panel and `is_busy`.
struct Activity {
    pending: VecDeque<QueueItem>,
    phase: Phase,
}

enum Phase {
    Idle,
    Running,
    Failed { message: String, since: Instant },
}

impl Activity {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            phase: Phase::Idle,
        }
    }

    fn is_busy(&self) -> bool {
        matches!(self.phase, Phase::Running) || !self.pending.is_empty()
    }

    fn status_view(&self) -> StatusView {
        if let Phase::Failed { message, since } = &self.phase
            && since.elapsed() < ERROR_DISPLAY
        {
            return StatusView::Error {
                message: message.clone(),
            };
        }
        if self.is_busy() {
            StatusView::Streaming
        } else {
            StatusView::Idle
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Pop the front item and mark the run active in one locked step, so the UI
/// can never observe the gap between "queue emptied" and "run started".
fn begin_next(activity: &Shared) -> Option<QueueItem> {
    let mut a = lock(activity);
    let item = a.pending.pop_front()?;
    a.phase = Phase::Running;
    Some(item)
}

/// Collapse to `Idle` only if still `Running` — a concurrent cancel may have
/// already moved us on. Leaves `pending` alone so queued follow-ups still run.
fn finish_ok(activity: &Shared) {
    let mut a = lock(activity);
    if matches!(a.phase, Phase::Running) {
        a.phase = Phase::Idle;
    }
}

/// Latch the error and drop queued follow-ups so they can't run after a
/// failure. Guarded on `Running` like `finish_ok`.
fn finish_err(activity: &Shared, message: String) {
    let mut a = lock(activity);
    if matches!(a.phase, Phase::Running) {
        a.pending.clear();
        a.phase = Phase::Failed {
            message,
            since: Instant::now(),
        };
    }
}

#[derive(Clone)]
pub(crate) struct QueueSender {
    activity: Shared,
    notify_tx: flume::Sender<()>,
}

pub(crate) struct QueueReceiver {
    activity: Shared,
    notify_rx: flume::Receiver<()>,
}

pub(crate) fn queue() -> (QueueSender, QueueReceiver) {
    let (notify_tx, notify_rx) = flume::bounded(1);
    let activity: Shared = Arc::new(Mutex::new(Activity::new()));
    (
        QueueSender {
            activity: Arc::clone(&activity),
            notify_tx,
        },
        QueueReceiver {
            activity,
            notify_rx,
        },
    )
}

impl QueueSender {
    /// Enqueue work. Any lingering error display is cleared: once the user
    /// submits again, the run is what matters.
    pub(crate) fn push(&self, item: QueueItem) {
        {
            let mut a = lock(&self.activity);
            if matches!(a.phase, Phase::Failed { .. }) {
                a.phase = Phase::Idle;
            }
            a.pending.push_back(item);
        }
        let _ = self.notify_tx.try_send(());
    }

    pub(crate) fn remove(&self, index: usize) -> Option<QueueItem> {
        let mut a = lock(&self.activity);
        (index < a.pending.len())
            .then(|| a.pending.remove(index))
            .flatten()
    }

    /// Cancel / session reset: forget queued work and any running or failed
    /// phase. Instant, so the spinner drops the moment the user hits Ctrl-C.
    pub(crate) fn reset(&self) {
        let mut a = lock(&self.activity);
        a.pending.clear();
        a.phase = Phase::Idle;
    }

    /// The agent channel died mid-run: record the failure it can no longer
    /// report itself.
    pub(crate) fn fail(&self, message: String) {
        let mut a = lock(&self.activity);
        a.pending.clear();
        a.phase = Phase::Failed {
            message,
            since: Instant::now(),
        };
    }

    pub(crate) fn is_busy(&self) -> bool {
        lock(&self.activity).is_busy()
    }

    pub(crate) fn status_view(&self) -> StatusView {
        lock(&self.activity).status_view()
    }

    pub(crate) fn len(&self) -> usize {
        lock(&self.activity).pending.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn text_messages(&self) -> Vec<String> {
        lock(&self.activity)
            .pending
            .iter()
            .filter(|item| item.visible_in_panel())
            .filter_map(|item| match item {
                QueueItem::Message { text, .. } => Some(text.clone()),
                QueueItem::Compact { .. } => None,
            })
            .collect()
    }

    pub(crate) fn panel_len(&self) -> usize {
        lock(&self.activity)
            .pending
            .iter()
            .filter(|item| item.visible_in_panel())
            .count()
    }

    pub(crate) fn panel_entries(&self) -> Vec<QueueEntry<'static>> {
        lock(&self.activity)
            .pending
            .iter()
            .filter(|item| item.visible_in_panel())
            .map(QueueItem::as_queue_entry)
            .collect()
    }

    // ---- test seams: simulate the agent side without a live loop ----

    /// Simulate the agent picking up the front item (pop + mark running).
    #[cfg(test)]
    pub(crate) fn begin_next(&self) -> Option<QueueItem> {
        begin_next(&self.activity)
    }

    /// Simulate a run beginning with nothing else to pop.
    #[cfg(test)]
    pub(crate) fn set_running(&self) {
        lock(&self.activity).phase = Phase::Running;
    }

    /// Simulate a clean run end (shares the prod path).
    #[cfg(test)]
    pub(crate) fn finish_ok(&self) {
        finish_ok(&self.activity);
    }

    /// Simulate the agent failing a run (shares the prod path).
    #[cfg(test)]
    pub(crate) fn finish_err(&self, message: impl Into<String>) {
        finish_err(&self.activity, message.into());
    }

    /// Land in a failed phase with a back-dated timestamp so error expiry can
    /// be exercised deterministically.
    #[cfg(test)]
    pub(crate) fn fail_with_age(&self, message: impl Into<String>, age: std::time::Duration) {
        let mut a = lock(&self.activity);
        a.pending.clear();
        a.phase = Phase::Failed {
            message: message.into(),
            since: Instant::now() - age,
        };
    }
}

impl QueueReceiver {
    /// Pop the front item and mark the run active in one locked step.
    pub(crate) fn begin_next(&self) -> Option<QueueItem> {
        begin_next(&self.activity)
    }

    /// The run ended cleanly.
    pub(crate) fn finish_ok(&self) {
        finish_ok(&self.activity);
    }

    /// The run failed.
    pub(crate) fn finish_err(&self, message: String) {
        finish_err(&self.activity, message);
    }

    pub(crate) async fn recv_notify(&self) -> Result<(), flume::RecvError> {
        self.notify_rx.recv_async().await
    }
}

impl InterruptSource for QueueReceiver {
    /// Mid-run fold: hand the current run the next queued item. Shrinks
    /// `pending` but leaves `phase` alone — the same run keeps going.
    fn poll(&self) -> Option<ExtractedCommand> {
        lock(&self.activity)
            .pending
            .pop_front()
            .map(QueueItem::into_extracted_command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn msg(displayed: bool) -> QueueItem {
        QueueItem::Message {
            text: "t".into(),
            image_count: 0,
            input: AgentInput {
                message: String::new(),
                mode: Default::default(),
                images: Vec::new(),
                preamble: Vec::new(),
                thinking: Default::default(),
                fast: false,
                workflow: false,
                prompt: None,
            },
            run_id: 0,
            displayed,
        }
    }

    #[test_case(msg(false),                       true  ; "deferred_message_visible")]
    #[test_case(msg(true),                        false ; "displayed_message_hidden")]
    #[test_case(QueueItem::Compact { run_id: 0 }, true  ; "compact_visible")]
    fn panel_visibility(item: QueueItem, visible: bool) {
        let (tx, _rx) = queue();
        tx.push(item);
        let expected = usize::from(visible);
        assert_eq!(tx.panel_len(), expected);
        assert_eq!(tx.panel_entries().len(), expected);
    }
}
