use std::sync::Arc;

use flume::Sender;
use futures_lite::future;
use maki_providers::provider::Provider;
use maki_providers::{Message, Model, ProviderEvent, RequestOptions};
use maki_storage::id::SessionRef;
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
        // The mirror is verbatim, so mid-turn it can end on an open tool call.
        // Providers reject that, so close them off on our own copy.
        let snapshot = self.shared_history.as_ref().map(|h| h.load_full());
        let mut messages = snapshot
            .as_ref()
            .map(|s| Vec::clone(&s.messages))
            .unwrap_or_default();
        maki_agent::close_dangling_tool_calls(&mut messages, maki_agent::UNAVAILABLE_RESULT);
        // The session's own prompt, so the side question reads the transcript
        // under the instructions it was written under.
        let system = snapshot
            .as_ref()
            .and_then(|s| s.frame.as_ref())
            .and_then(|f| f.prefix())
            .unwrap_or(BTW_FALLBACK_SYSTEM)
            .to_owned();
        messages.push(btw_question(&question));

        let (tx, rx) = flume::bounded(64);
        self.btw_modal.open(&question, rx);

        let session_id = SessionRef::from(self.state.session.id);
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
    session_id: Option<SessionRef>,
) {
    let (event_tx, event_rx) = flume::unbounded();
    let tools = Value::Array(vec![]);
    let messages = maki_providers::adapt_images_for_model(&model, &messages).await;

    let stream_fut = provider.stream_message(
        &model,
        &messages,
        &system,
        &tools,
        &event_tx,
        RequestOptions::default(),
        session_id.as_ref(),
    );

    let forward_fut = async {
        while let Ok(event) = event_rx.recv_async().await {
            let event = match event {
                ProviderEvent::TextDelta { text } => BtwEvent::TextDelta(text),
                ProviderEvent::ThinkingDelta { text } => BtwEvent::ThinkingDelta(text),
                _ => continue,
            };
            if btw_tx.send(event).is_err() {
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
    use arc_swap::ArcSwap;
    use maki_agent::HistorySnapshot;
    use maki_agent::prompt::{PromptId, Slot};
    use maki_agent::tools::ToolRegistry;
    use maki_lua::PluginHost;
    use maki_providers::provider::BoxFuture;
    use maki_providers::{AgentError, ModelInfo, StreamResponse};
    use maki_storage::frame::{PromptFacts, StoredFrame};
    use std::future;
    use test_case::test_case;

    const Q: &str = "why sqlite?";
    const FRAME_POLICY: &str = "Use the active conversation policy.";

    struct RecordingProvider(Sender<(String, Value)>);

    impl Provider for RecordingProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            system: &'a str,
            tools: &'a Value,
            _: &'a Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.0.send((system.to_owned(), tools.clone())).unwrap();
                future::pending().await
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn rendered_frame() -> StoredFrame {
        StoredFrame::new(FRAME_POLICY.into(), String::new(), None, Vec::new())
    }

    /// An imported session holds a frame with facts and no prompt until its
    /// first run, and an empty system is a 400 on Anthropic.
    #[test_case(None, BTW_FALLBACK_SYSTEM ; "native_fallback")]
    #[test_case(Some(rendered_frame()), FRAME_POLICY ; "stored_frame_precedence")]
    #[test_case(Some(StoredFrame::facts_only(PromptFacts::default())), BTW_FALLBACK_SYSTEM ; "imported_before_first_run")]
    fn side_question_uses_the_selected_policy(frame: Option<StoredFrame>, expected: &str) {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "persona",
            r#"
            local calls = 0
            maki.api.set_prompt({ slot = "identity", content = function()
                calls += 1
                return tostring(calls)
            end })
            "#,
        )
        .unwrap();
        let mut app = crate::app::tests::test_app();
        app.lua_event_handle = host.event_handle();
        if let Some(frame) = frame {
            let mut snapshot = HistorySnapshot::new(Vec::new());
            snapshot.frame = Some(Arc::new(frame));
            app.shared_history = Some(Arc::new(ArcSwap::from_pointee(snapshot)));
        }
        let (sent, requests) = flume::unbounded();
        app.start_btw(
            Q.into(),
            Arc::new(RecordingProvider(sent)),
            crate::components::test_model(),
        );
        let (system, tools) = smol::block_on(requests.recv_async()).unwrap();
        assert_eq!(system, expected);
        assert!(tools.as_array().unwrap().is_empty());
        let prompts = host.event_handle().collect_prompt_slots();
        assert_eq!(
            prompts.get(PromptId::System, Slot::Identity)[0].content,
            "1"
        );
    }

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
