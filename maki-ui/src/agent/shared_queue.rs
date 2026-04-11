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

use crate::components::input::Submission;
use crate::components::queue_panel::QueueEntry;
use crate::theme;

const COMPACT_LABEL: &str = "/compact";

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
                    .queue_compact
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
            .filter_map(|item| match item {
                QueueItem::Message { text, .. } => Some(text.clone()),
                QueueItem::Compact { .. } => None,
            })
            .collect()
    }

    pub(crate) fn entries(&self) -> Vec<QueueEntry<'static>> {
        lock(&self.items)
            .iter()
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

    #[test]
    fn last_sender_drop_closes_notify_channel() {
        let (tx, rx) = queue();
        let tx2 = tx.clone();

        drop(tx);
        assert_eq!(rx.notify_rx.try_recv(), Err(flume::TryRecvError::Empty));

        drop(tx2);
        assert_eq!(
            rx.notify_rx.try_recv(),
            Err(flume::TryRecvError::Disconnected)
        );
    }
}
