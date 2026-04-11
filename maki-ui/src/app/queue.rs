//! Queue for messages typed while the agent is busy.
//!
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

    pub(crate) fn remove(&mut self, index: usize) {
        if let Some(ref shared) = self.shared
            && shared.remove(index).is_some()
        {
            self.clamp_focus();
        }
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

    pub(crate) fn remove_focused(&mut self) {
        if let Some(sel) = self.focus {
            self.remove(sel);
        }
    }

    pub(crate) fn entries(&self) -> Vec<QueueEntry<'static>> {
        self.shared.as_ref().map_or(vec![], |s| s.entries())
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
        });
    }

    pub(super) fn queue_compact(&mut self) {
        let Some(ref shared) = self.queue.shared else {
            return;
        };
        shared.push(QueueItem::Compact {
            run_id: self.run_id,
        });
    }

    pub(super) fn on_queue_item_consumed(&mut self, text: &str, image_count: usize) {
        self.main_chat().flush();
        self.main_chat()
            .push_user_message(format_with_images(text, image_count));
        self.main_chat().enable_auto_scroll();
    }

    pub(super) fn start_from_queue(&mut self, msg: &QueuedMessage) -> Vec<super::Action> {
        self.status = super::Status::Streaming;
        vec![super::Action::SendMessage(Box::new(
            self.build_agent_input(msg),
        ))]
    }
}
