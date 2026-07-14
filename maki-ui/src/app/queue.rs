//! Queue for messages typed while the agent is busy.

use super::{App, format_with_images};

use crate::agent::shared_queue::{QueueItem, QueueSender};
use crate::components::StatusView;
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

    pub(crate) fn is_busy(&self) -> bool {
        self.shared.as_ref().is_some_and(|s| s.is_busy())
    }

    pub(crate) fn status_view(&self) -> StatusView {
        self.shared
            .as_ref()
            .map_or(StatusView::Idle, |s| s.status_view())
    }

    pub(crate) fn reset(&mut self) {
        if let Some(ref s) = self.shared {
            s.reset();
        }
        self.focus = None;
    }

    pub(crate) fn fail(&mut self, message: String) {
        if let Some(ref s) = self.shared {
            s.fail(message);
        }
        self.focus = None;
    }

    #[cfg(test)]
    pub(crate) fn begin_next(&self) -> Option<QueueItem> {
        self.shared.as_ref().and_then(|s| s.begin_next())
    }

    #[cfg(test)]
    pub(crate) fn set_running(&self) {
        if let Some(ref s) = self.shared {
            s.set_running();
        }
    }

    #[cfg(test)]
    pub(crate) fn finish_ok(&self) {
        if let Some(ref s) = self.shared {
            s.finish_ok();
        }
    }

    #[cfg(test)]
    pub(crate) fn finish_err(&self, message: impl Into<String>) {
        if let Some(ref s) = self.shared {
            s.finish_err(message);
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_with_age(&self, message: impl Into<String>, age: std::time::Duration) {
        if let Some(ref s) = self.shared {
            s.fail_with_age(message, age);
        }
    }

    /// Focus is a cursor into the panel; the panel is derived from the shared
    /// queue, which the agent can empty (e.g. on error) without going through
    /// the UI. Filtering here keeps focus from dangling past a vanished row.
    pub(crate) fn focus(&self) -> Option<usize> {
        self.focus.filter(|&i| i < self.panel_len())
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
    }

    /// Drop a queued item by index.
    pub(super) fn remove_queued(&mut self, index: usize) {
        self.queue.remove(index);
    }

    pub(super) fn remove_focused_queued(&mut self) {
        self.queue.remove_focused();
    }

    pub(super) fn queue_compact(&mut self) {
        let Some(ref shared) = self.queue.shared else {
            return;
        };
        shared.push(QueueItem::Compact {
            run_id: self.run_id,
        });
    }

    /// Agent reached a deferred message: draw the bubble now that it's
    /// actually running, instead of when the user merely typed it.
    pub(super) fn on_queue_item_consumed(&mut self, text: &str, image_count: usize) {
        self.main_chat()
            .show_user_message(format_with_images(text, image_count));
    }

    /// Immediate path: kick off the agent and draw the bubble in the same
    /// frame, so the user sees their message land where it will stay.
    pub(super) fn start_from_queue(&mut self, msg: &QueuedMessage) -> Vec<super::Action> {
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
