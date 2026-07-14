//! Queue for messages typed while the agent is busy.

use super::{App, format_with_images};

use crate::agent::shared_queue::{QueueItem, QueueSender};
use crate::components::queue_panel::QueueEntry;

pub(crate) use crate::agent::shared_queue::QueuedMessage;

#[derive(Default)]
pub(crate) struct MessageQueue {
    shared: Option<QueueSender>,
    focus: Option<usize>,
}

impl MessageQueue {
    pub(crate) fn set_shared(&mut self, shared: QueueSender) {
        self.shared = Some(shared);
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.shared.as_ref().is_none_or(|s| s.is_empty())
    }

    pub(crate) fn len(&self) -> usize {
        self.shared.as_ref().map_or(0, |s| s.len())
    }

    pub(crate) fn remove(&mut self, index: usize) -> Option<QueueItem> {
        let removed = self.shared.as_ref().and_then(|s| s.remove(index));
        if removed.is_some() {
            self.clamp_focus();
        }
        removed
    }

    pub(crate) fn clear(&mut self) {
        if let Some(ref shared) = self.shared {
            shared.clear();
        }
        self.focus = None;
    }

    pub(crate) fn focus(&self) -> Option<usize> {
        self.focus
    }

    pub(crate) fn set_focus(&mut self) {
        self.set_focus_at(0);
    }

    pub(crate) fn unfocus(&mut self) {
        self.focus = None;
    }

    pub(crate) fn move_focus_up(&mut self) {
        if let Some(sel) = self.focus
            && sel > 0
        {
            self.focus = Some(sel - 1);
        }
    }

    pub(crate) fn move_focus_down(&mut self) {
        if let Some(sel) = self.focus {
            let len = self.len();
            if sel + 1 < len {
                self.focus = Some(sel + 1);
            }
        }
    }

    pub(crate) fn remove_focused(&mut self) -> Option<QueueItem> {
        self.focus.and_then(|sel| self.remove(sel))
    }

    pub(crate) fn panel_len(&self) -> usize {
        self.shared.as_ref().map_or(0, |s| s.panel_len())
    }

    pub(crate) fn panel_entries(&self) -> Vec<QueueEntry<'static>> {
        self.shared.as_ref().map_or(vec![], |s| s.panel_entries())
    }

    pub(crate) fn text_messages(&self) -> Vec<String> {
        self.shared.as_ref().map_or(vec![], |s| s.text_messages())
    }

    fn clamp_focus(&mut self) {
        let len = self.len();
        self.focus = match self.focus {
            Some(_) if len == 0 => None,
            Some(sel) if sel >= len => Some(len - 1),
            other => other,
        };
    }

    pub(crate) fn set_focus_at(&mut self, index: usize) {
        if index < self.len() {
            self.focus = Some(index);
        }
    }
}

impl App {
    /// Deferred path: the agent is busy, so park the message and let
    /// `QueueItemConsumed` draw it once the agent picks it up.
    pub(super) fn queue_and_notify(&mut self, msg: QueuedMessage) {
        let Some(ref shared) = self.queue.shared else {
            return;
        };
        let input = self.build_agent_input(&msg);
        shared.push(QueueItem::Message {
            text: msg.text,
            image_count: msg.images.len(),
            input,
            run_id: self.run_id,
            displayed: false,
        });
        self.queued_count += 1;
    }

    /// Drop a queued item by index, keeping `queued_count` in lockstep so the
    /// spinner and submit gate don't count a message the user just removed.
    pub(super) fn remove_queued(&mut self, index: usize) {
        if matches!(self.queue.remove(index), Some(QueueItem::Message { .. })) {
            self.queued_count = self.queued_count.saturating_sub(1);
        }
    }

    pub(super) fn remove_focused_queued(&mut self) {
        if matches!(self.queue.remove_focused(), Some(QueueItem::Message { .. })) {
            self.queued_count = self.queued_count.saturating_sub(1);
        }
    }

    pub(super) fn queue_compact(&mut self) {
        let Some(ref shared) = self.queue.shared else {
            return;
        };
        shared.push(QueueItem::Compact {
            run_id: self.run_id,
        });
    }

    /// Agent reached a deferred message: draw the bubble and mark one fewer
    /// message pending, so the spinner and submit gate stay honest.
    pub(super) fn on_queue_item_consumed(&mut self, text: &str, image_count: usize) {
        self.queued_count = self.queued_count.saturating_sub(1);
        self.main_chat()
            .show_user_message(format_with_images(text, image_count));
    }

    /// Immediate path: kick off the agent and draw the bubble in the same
    /// frame, so the user sees their message land where it will stay.
    pub(super) fn start_from_queue(&mut self, msg: &QueuedMessage) -> Vec<super::Action> {
        self.start_run();
        if let Some(ref handle) = self.lua_event_handle {
            handle.fire_autocmd("TurnStart", serde_json::json!({}));
        }
        self.main_chat()
            .show_user_message(format_with_images(&msg.text, msg.images.len()));
        vec![super::Action::SendMessage(Box::new(
            self.build_agent_input(msg),
        ))]
    }
}
