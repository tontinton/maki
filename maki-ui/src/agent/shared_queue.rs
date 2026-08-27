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

use maki_agent::{AgentInput, AgentMode, ExtractedCommand, ImageSource, InterruptSource};
use maki_providers::Message;

use crate::components::input::Submission;
use crate::components::queue_panel::QueueEntry;
use crate::theme;

const COMPACT_LABEL: &str = "/compact";

type Items = Arc<Mutex<VecDeque<QueueItem>>>;
/// What a message must agree on with its neighbours to share their turn.
type BatchKey = (AgentMode, bool);

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

pub(crate) struct QueuedInput {
    pub(crate) text: String,
    pub(crate) image_count: usize,
    pub(crate) input: AgentInput,
    pub(crate) run_id: u64,
    /// `true` when the UI already drew the bubble (immediate dispatch).
    /// The agent then skips `QueueItemConsumed` so we don't draw it twice.
    /// `false` when the user typed while the agent was busy: the UI waits
    /// for `QueueItemConsumed` before drawing.
    pub(crate) displayed: bool,
}

impl QueuedInput {
    /// The set of messages this one may share a turn with, or `None` when it
    /// has to run alone: the agent resolves one MCP prompt per run. `mode` is
    /// a permission boundary and `workflow` picks the tool catalog, so a
    /// message queued under either may not execute under another one.
    ///
    /// Destructured on purpose: a new `AgentInput` field then has to be
    /// classified here instead of silently merging across.
    fn batch_key(&self) -> Option<BatchKey> {
        let AgentInput {
            mode,
            workflow,
            prompt,
            message: _,
            images: _,
            preamble: _,
            thinking: _,
            fast: _,
        } = &self.input;
        prompt.is_none().then(|| (mode.clone(), *workflow))
    }
}

pub(crate) enum QueueItem {
    Message(QueuedInput),
    Compact { run_id: u64 },
}

/// One turn's worth of work. Plain messages typed back to back under the same
/// mode and workflow travel together, so the whole burst costs one run and one
/// request. Everything else travels alone and keeps its place, since
/// `/compact` rewrites the history the later messages land in.
pub(crate) enum QueueRun {
    Messages(Vec<QueuedInput>),
    Compact { run_id: u64 },
}

impl QueueRun {
    /// Forgets whatever the user cancelled before we got to it, and reports
    /// the id the rest rides on: the newest one, the only run the UI still
    /// considers current.
    pub(crate) fn drop_cancelled(&mut self, min_run_id: u64) -> Option<u64> {
        match self {
            Self::Messages(messages) => {
                messages.retain(|queued| queued.run_id >= min_run_id);
                Some(messages.last()?.run_id)
            }
            Self::Compact { run_id } => (*run_id >= min_run_id).then_some(*run_id),
        }
    }
}

impl QueueItem {
    fn as_queue_entry(&self) -> QueueEntry<'static> {
        match self {
            Self::Message(queued) => QueueEntry {
                text: Cow::Owned(queued.text.clone()),
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

    fn batch_key(&self) -> Option<BatchKey> {
        match self {
            Self::Message(queued) => queued.batch_key(),
            Self::Compact { .. } => None,
        }
    }

    /// Immediate-dispatch messages already sit in the chat, so hiding them
    /// here stops the panel from reserving a row the agent is about to free,
    /// which used to make the bubble hop up by one frame.
    fn visible_in_panel(&self) -> bool {
        match self {
            Self::Message(queued) => !queued.displayed,
            Self::Compact { .. } => true,
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
                QueueItem::Message(queued) => Some(queued.text.clone()),
                QueueItem::Compact { .. } => None,
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
    /// Takes the next item plus every message queued right behind it that
    /// shares its batch key.
    pub(crate) fn pop_run(&self) -> Option<QueueRun> {
        let mut items = lock(&self.items);
        let first = match items.pop_front()? {
            QueueItem::Compact { run_id } => return Some(QueueRun::Compact { run_id }),
            QueueItem::Message(first) => first,
        };
        let key = first.batch_key();
        let mut run = vec![first];
        while key.is_some() && items.front().and_then(QueueItem::batch_key) == key {
            let Some(QueueItem::Message(next)) = items.pop_front() else {
                break;
            };
            run.push(next);
        }
        Some(QueueRun::Messages(run))
    }

    /// Runs `publish` under the queue lock, so a drain event can never
    /// interleave with a concurrent push.
    pub(crate) fn publish_if_empty(&self, publish: impl FnOnce()) {
        let items = lock(&self.items);
        if items.is_empty() {
            publish();
        }
    }

    pub(crate) async fn recv_notify(&self) -> Result<(), flume::RecvError> {
        self.notify_rx.recv_async().await
    }
}

impl InterruptSource for QueueReceiver {
    fn poll(&self) -> Option<ExtractedCommand> {
        Some(match self.pop_run()? {
            QueueRun::Compact { .. } => ExtractedCommand::Compact,
            QueueRun::Messages(run) => {
                ExtractedCommand::Interrupt(run.into_iter().map(|queued| queued.input).collect())
            }
        })
    }
}

/// Folds a run of queued messages into one agent input. The last message
/// drives the run and the earlier ones ride in front of it as their own user
/// messages, so each keeps its images and the model answers the burst in one
/// request. The run shares one mode and one workflow by construction, so only
/// the preferences (thinking, fast) come from the last message, the user's
/// most recent intent.
pub(crate) fn merge_inputs(mut inputs: Vec<AgentInput>) -> Option<AgentInput> {
    let mut last = inputs.pop()?;
    let mut preamble = Vec::new();
    for earlier in inputs {
        preamble.extend(earlier.preamble);
        let message = Message::user_with_images(earlier.message, earlier.images);
        // An input with neither text nor images would become a user message
        // with no content at all, which providers reject.
        if !message.content.is_empty() {
            preamble.push(message);
        }
    }
    preamble.append(&mut last.preamble);
    last.preamble = preamble;
    Some(last)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::Barrier;
    use std::thread;

    use maki_agent::{ImageMediaType, McpPromptRef};

    use super::*;
    use test_case::test_case;

    const FIRST: &str = "first";
    const SECOND: &str = "second";
    const THIRD: &str = "third";
    const PROMPT_NAME: &str = "server/review";
    const PLAN_PATH: &str = "plan.md";
    const NOT_AN_INTERRUPT: &str = "expected an interrupt carrying the queued messages";
    /// One image block plus one text block.
    const BLOCKS_PER_MESSAGE: usize = 2;

    fn input(message: &str) -> AgentInput {
        AgentInput {
            message: message.into(),
            mode: Default::default(),
            images: Vec::new(),
            preamble: Vec::new(),
            thinking: Default::default(),
            fast: false,
            workflow: false,
            prompt: None,
        }
    }

    fn queued(text: &str, run_id: u64) -> QueuedInput {
        QueuedInput {
            text: text.into(),
            image_count: 0,
            input: input(text),
            run_id,
            displayed: false,
        }
    }

    fn message(text: &str) -> QueueItem {
        QueueItem::Message(queued(text, 0))
    }

    fn plan_message(text: &str) -> QueueItem {
        let mut queued = queued(text, 0);
        queued.input.mode = AgentMode::Plan(PLAN_PATH.into());
        QueueItem::Message(queued)
    }

    fn workflow_message(text: &str) -> QueueItem {
        let mut queued = queued(text, 0);
        queued.input.workflow = true;
        QueueItem::Message(queued)
    }

    fn prompt_message(text: &str) -> QueueItem {
        let mut queued = queued(text, 0);
        queued.input.prompt = Some(Box::new(McpPromptRef {
            qualified_name: PROMPT_NAME.into(),
            arguments: Default::default(),
        }));
        QueueItem::Message(queued)
    }

    fn compact() -> QueueItem {
        QueueItem::Compact { run_id: 0 }
    }

    /// Drains the queue and names every run it hands over: the texts of a
    /// burst of messages, or `/compact` on its own.
    fn runs(rx: &QueueReceiver) -> Vec<Vec<String>> {
        let mut runs = Vec::new();
        while let Some(run) = rx.pop_run() {
            runs.push(match run {
                QueueRun::Messages(messages) => {
                    messages.into_iter().map(|queued| queued.text).collect()
                }
                QueueRun::Compact { .. } => vec![COMPACT_LABEL.into()],
            });
        }
        runs
    }

    #[test_case(message(FIRST), true  ; "deferred_message_visible")]
    #[test_case(QueueItem::Message(QueuedInput { displayed: true, ..queued(FIRST, 0) }), false ; "displayed_message_hidden")]
    #[test_case(compact(), true  ; "compact_visible")]
    fn panel_visibility(item: QueueItem, visible: bool) {
        let (tx, _rx) = queue();
        tx.push(item);
        let expected = usize::from(visible);
        assert_eq!(tx.panel_len(), expected);
        assert_eq!(tx.panel_entries().len(), expected);
    }

    #[test]
    fn nonempty_queue_does_not_publish_drain() {
        let (tx, rx) = queue();
        tx.push(message(FIRST));
        let called = Cell::new(false);

        rx.publish_if_empty(|| called.set(true));
        assert!(!called.get());
    }

    #[test]
    fn drain_publication_is_serialized_with_push() {
        let (tx, rx) = queue();
        let barrier = Arc::new(Barrier::new(2));
        let order = Arc::new(Mutex::new(Vec::new()));
        let worker_barrier = Arc::clone(&barrier);
        let worker_order = Arc::clone(&order);
        let worker = thread::spawn(move || {
            worker_barrier.wait();
            tx.push(message(FIRST));
            lock(&worker_order).push("push");
        });

        rx.publish_if_empty(|| {
            barrier.wait();
            lock(&order).push("drain");
        });
        worker.join().unwrap();

        assert_eq!(*lock(&order), ["drain", "push"]);
    }

    #[test_case(vec![message(FIRST), message(SECOND), message(THIRD)], vec![vec![FIRST, SECOND, THIRD]] ; "adjacent_messages_ride_together")]
    #[test_case(vec![message(FIRST), compact(), message(SECOND)], vec![vec![FIRST], vec![COMPACT_LABEL], vec![SECOND]] ; "compact_splits_the_drain_and_keeps_its_place")]
    #[test_case(vec![message(FIRST), prompt_message(SECOND), message(THIRD)], vec![vec![FIRST], vec![SECOND], vec![THIRD]] ; "mcp_prompt_runs_alone")]
    #[test_case(vec![message(FIRST)], vec![vec![FIRST]] ; "single_message_unchanged")]
    #[test_case(vec![plan_message(FIRST), plan_message(SECOND)], vec![vec![FIRST, SECOND]] ; "one_mode_rides_together")]
    #[test_case(vec![plan_message(FIRST), message(SECOND)], vec![vec![FIRST], vec![SECOND]] ; "mode_change_splits_the_drain")]
    #[test_case(vec![message(FIRST), workflow_message(SECOND), message(THIRD)], vec![vec![FIRST], vec![SECOND], vec![THIRD]] ; "workflow_change_splits_the_drain")]
    fn pop_run_groups_the_queue_into_turns(items: Vec<QueueItem>, expected: Vec<Vec<&str>>) {
        let (tx, rx) = queue();
        for item in items {
            tx.push(item);
        }

        assert_eq!(runs(&rx), expected);
        assert!(tx.is_empty());
    }

    #[test_case(0, Some(2) ; "nothing_cancelled")]
    #[test_case(2, Some(2) ; "cancelled_message_dropped")]
    #[test_case(3, None    ; "whole_run_cancelled")]
    fn drop_cancelled_keeps_the_newest_run_id(min_run_id: u64, expected: Option<u64>) {
        let mut run = QueueRun::Messages(vec![queued(FIRST, 1), queued(SECOND, 2)]);

        assert_eq!(run.drop_cancelled(min_run_id), expected);
    }

    #[test]
    fn poll_hands_the_whole_message_run_to_one_turn() {
        let (tx, rx) = queue();
        for text in [FIRST, SECOND, THIRD] {
            tx.push(message(text));
        }

        let Some(ExtractedCommand::Interrupt(inputs)) = rx.poll() else {
            panic!("{NOT_AN_INTERRUPT}");
        };
        let messages: Vec<_> = inputs.iter().map(|i| i.message.as_str()).collect();

        assert_eq!(messages, [FIRST, SECOND, THIRD]);
        assert!(rx.poll().is_none());
    }

    #[test_case(&[FIRST] ; "single_message_stays_alone")]
    #[test_case(&[FIRST, SECOND, THIRD] ; "earlier_messages_ride_in_the_preamble")]
    fn merge_inputs_keeps_every_message_with_its_own_image(texts: &[&str]) {
        let inputs = texts
            .iter()
            .map(|text| AgentInput {
                images: vec![ImageSource::new(ImageMediaType::Png, Arc::from(*text))],
                ..input(text)
            })
            .collect();

        let merged = merge_inputs(inputs).unwrap();

        let (last, earlier) = texts.split_last().unwrap();
        assert_eq!(merged.message, *last);
        assert_eq!(merged.images.len(), 1);
        let preamble: Vec<_> = merged
            .preamble
            .iter()
            .map(|m| (m.user_text(), m.content.len()))
            .collect();
        let expected: Vec<_> = earlier
            .iter()
            .map(|text| (Some(*text), BLOCKS_PER_MESSAGE))
            .collect();
        assert_eq!(preamble, expected);
    }
}
