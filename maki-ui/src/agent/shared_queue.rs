//! Queue of work handed from the UI to the agent loop.
//!
//! Shutdown rides on `Drop`: when the last [`QueueSender`] goes away, flume
//! closes the notify channel, so the receiver's `recv_notify` wakes with an
//! `Err` and the agent loop falls out of its main loop on its own. That way
//! nobody needs a separate "please stop" flag, and callers can't forget to
//! set it.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use maki_agent::{AgentInput, ExtractedCommand, ImageSource, InterruptSource};
use maki_storage::tree::NodeRef;

use crate::components::input::Submission;
use crate::components::queue_panel::QueueEntry;
use crate::theme;

const COMPACT_LABEL: &str = "/compact";
const BRANCH_SUMMARY_LABEL: &str = "/summarize branch";

type Items = Arc<Mutex<VecDeque<QueueItem>>>;

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
    /// Branch-summary (§6): generate a narrative for the abandoned branch and
    /// append a `SummaryRecord` with `SummaryKind::Branch`. `parent` is the
    /// landing node; `fold_from_id` is the abandoned tip (provenance only).
    BranchSummary {
        run_id: u64,
        parent: NodeRef,
        fold_from_id: NodeRef,
    },
}

impl QueueItem {
    pub(crate) fn run_id(&self) -> u64 {
        match self {
            Self::Message { run_id, .. }
            | Self::Compact { run_id }
            | Self::BranchSummary { run_id, .. } => *run_id,
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
            Self::BranchSummary { .. } => QueueEntry {
                text: Cow::Borrowed(BRANCH_SUMMARY_LABEL),
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
            Self::BranchSummary {
                run_id,
                parent,
                fold_from_id,
            } => ExtractedCommand::BranchSummary {
                run_id,
                parent,
                fold_from_id,
            },
        }
    }

    fn visible_in_panel(&self) -> bool {
        match self {
            Self::Message { displayed, .. } => !displayed,
            Self::Compact { .. } | Self::BranchSummary { .. } => true,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub(crate) struct QueueSender {
    items: Items,
    notify_tx: flume::Sender<()>,
}

pub(crate) struct QueueReceiver {
    items: Items,
    notify_rx: flume::Receiver<()>,
}

pub(crate) fn queue() -> (QueueSender, QueueReceiver) {
    let (notify_tx, notify_rx) = flume::bounded(1);
    let items: Items = Arc::new(Mutex::new(VecDeque::new()));
    (
        QueueSender {
            items: Arc::clone(&items),
            notify_tx,
        },
        QueueReceiver { items, notify_rx },
    )
}

impl QueueSender {
    pub(crate) fn push(&self, entry: QueueItem) {
        lock(&self.items).push_back(entry);
        let _ = self.notify_tx.try_send(());
    }

    pub(crate) fn remove(&self, index: usize) -> Option<QueueItem> {
        let mut items = lock(&self.items);
        (index < items.len()).then(|| items.remove(index)).flatten()
    }

    pub(crate) fn len(&self) -> usize {
        lock(&self.items).len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn clear(&self) {
        lock(&self.items).clear();
    }

    pub(crate) fn text_messages(&self) -> Vec<String> {
        lock(&self.items)
            .iter()
            .filter(|item| item.visible_in_panel())
            .filter_map(|item| match item {
                QueueItem::Message { text, .. } => Some(text.clone()),
                QueueItem::Compact { .. } | QueueItem::BranchSummary { .. } => None,
            })
            .collect()
    }

    pub(crate) fn panel_len(&self) -> usize {
        lock(&self.items)
            .iter()
            .filter(|item| item.visible_in_panel())
            .count()
    }

    pub(crate) fn panel_entries(&self) -> Vec<QueueEntry<'static>> {
        lock(&self.items)
            .iter()
            .filter(|item| item.visible_in_panel())
            .map(QueueItem::as_queue_entry)
            .collect()
    }
}

impl QueueReceiver {
    pub(crate) fn pop(&self) -> Option<QueueItem> {
        lock(&self.items).pop_front()
    }

    pub(crate) async fn recv_notify(&self) -> Result<(), flume::RecvError> {
        self.notify_rx.recv_async().await
    }
}

impl InterruptSource for QueueReceiver {
    fn poll(&self) -> Option<ExtractedCommand> {
        self.pop().map(QueueItem::into_extracted_command)
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
    #[test_case(
        QueueItem::BranchSummary {
            run_id: 0,
            parent: NodeRef::Msg(serde_json::from_str("\"msg_x\"").unwrap()),
            fold_from_id: NodeRef::Msg(serde_json::from_str("\"msg_y\"").unwrap()),
        },
        true
        ; "branch_summary_visible"
    )]
    fn panel_visibility(item: QueueItem, visible: bool) {
        let (tx, _rx) = queue();
        tx.push(item);
        let expected = usize::from(visible);
        assert_eq!(tx.panel_len(), expected);
        assert_eq!(tx.panel_entries().len(), expected);
    }
}
