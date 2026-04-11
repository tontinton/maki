use std::sync::Arc;

use flume::Sender;
use futures_lite::future;
use maki_providers::provider::Provider;
use maki_providers::{Message, Model, ProviderEvent, ThinkingConfig};
use serde_json::Value;

use crate::components::btw_modal::BtwEvent;

use super::App;

const BTW_SYSTEM: &str = "Answer the user's question concisely. No tools available.";

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

        let (tx, rx) = flume::bounded(64);
        self.btw_modal.open(&question, rx);
        messages.push(Message::user(question));

        let session_id = self.state.session.id.clone();
        smol::spawn(run_btw(provider, model, messages, tx, Some(session_id))).detach();
    }
}

async fn run_btw(
    provider: Arc<dyn Provider>,
    model: Model,
    messages: Vec<Message>,
    btw_tx: Sender<BtwEvent>,
    session_id: Option<String>,
) {
    let (event_tx, event_rx) = flume::unbounded();
    let tools = Value::Array(vec![]);

    let stream_fut = provider.stream_message(
        &model,
        &messages,
        BTW_SYSTEM,
        &tools,
        &event_tx,
        ThinkingConfig::Off,
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
