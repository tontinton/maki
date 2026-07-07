use std::sync::Arc;

use flume::Sender;
use futures_lite::future;
use maki_providers::provider::Provider;
use maki_providers::{Message, Model, ProviderEvent, RequestOptions};
use serde_json::Value;

use crate::components::btw_modal::BtwEvent;

use super::App;

const BTW_REMINDER: &str = "<system-reminder>\nThis is a side question. Answer it directly in a \
single response.\n- You have NO tools: you cannot read files, run commands, or take any action.\n\
- One-off response: there are no follow-up turns.\n- Answer ONLY from the existing conversation \
context.\n- Never say \"Let me...\", \"I'll now...\", or promise any action.\n- If you don't know, \
say so; do not offer to look it up.\n</system-reminder>";

const BTW_FALLBACK_SYSTEM: &str = "You are a helpful coding assistant. Answer concisely \
from the conversation context.";

/// The reminder leads so the model treats the question as a quick aside, not a task to act on.
pub(crate) fn btw_question(question: &str) -> Message {
    Message::user(format!("{BTW_REMINDER}\n\n{question}"))
}

impl App {
    pub(crate) fn start_btw(
        &mut self,
        question: String,
        provider: Arc<dyn Provider>,
        model: Model,
    ) {
        let mut messages = self
            .shared_history
            .as_ref()
            .map(|h| Vec::clone(&h.load()))
            .unwrap_or_default();
        let system = self
            .btw_system
            .as_ref()
            .map(|s| String::clone(&s.load()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| BTW_FALLBACK_SYSTEM.to_string());
        messages.push(btw_question(&question));

        let (tx, rx) = flume::bounded(64);
        self.btw_modal.open(&question, rx);

        let session_id = self.state.session.id.clone();
        smol::spawn(run_btw(
            provider,
            model,
            system,
            messages,
            tx,
            Some(session_id),
        ))
        .detach();
    }
}

async fn run_btw(
    provider: Arc<dyn Provider>,
    model: Model,
    system: String,
    messages: Vec<Message>,
    btw_tx: Sender<BtwEvent>,
    session_id: Option<String>,
) {
    let (event_tx, event_rx) = flume::unbounded();
    let tools = Value::Array(vec![]);
    let messages = maki_providers::adapt_images_for_model(&model, &messages);

    let stream_fut = provider.stream_message(
        &model,
        &messages,
        &system,
        &tools,
        &event_tx,
        RequestOptions::default(),
        session_id.as_deref(),
    );

    let forward_fut = async {
        while let Ok(event) = event_rx.recv_async().await {
            let delta = match event {
                ProviderEvent::TextDelta { text } | ProviderEvent::ThinkingDelta { text } => text,
                _ => continue,
            };
            if btw_tx.send(BtwEvent::TextDelta(delta)).is_err() {
                return;
            }
        }
    };

    let (result, _) = future::zip(stream_fut, forward_fut).await;

    match result {
        Ok(_) => {
            let _ = btw_tx.send(BtwEvent::Done);
        }
        Err(e) => {
            let _ = btw_tx.send(BtwEvent::Error(e.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: &str = "why sqlite?";

    fn user_text(msg: &Message) -> String {
        msg.content
            .iter()
            .filter_map(|b| match b {
                maki_providers::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn injects_reminder_before_question() {
        let text = user_text(&btw_question(Q));
        assert!(text.starts_with(BTW_REMINDER), "reminder leads the message");
        assert!(text.ends_with(Q), "question trails the message");
    }
}
