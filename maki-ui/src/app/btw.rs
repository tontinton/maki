use std::sync::Arc;

use flume::Sender;
use maki_providers::provider::Provider;
use maki_providers::{Message, Model, ProviderEvent, RequestOptions};
use maki_storage::id::SessionRef;
use maki_storage::sessions::SessionTools;
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

const NO_TOOLS: Value = Value::Array(Vec::new());

/// The reminder leads so the model treats the question as a quick aside, not a task to act on.
fn btw_question(question: &str) -> Message {
    Message::user(format!("{BTW_REMINDER}\n\n{question}"))
}

/// The tools ride along only for the cache; the reminder keeps the model off
/// them. When it reaches for one anyway, say so instead of showing half an answer.
fn reached_for_tool(name: &str) -> String {
    format!(
        "The model tried to call `{name}`, but a side question has no tools. Ask it another way, \
         or send it as a normal message."
    )
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
        messages.push(btw_question(&question));
        // The session's own prompt, so the side question reads the transcript
        // under the instructions it was written under.
        let system = snapshot
            .as_ref()
            .and_then(|s| s.frame.as_ref())
            .and_then(|f| f.prefix())
            .unwrap_or(BTW_FALLBACK_SYSTEM)
            .to_owned();
        // Anthropic keys its cache on tools, system, then messages under one
        // thinking setting, so the request repeats the session's tools and
        // thinking (`tool_choice: none` would void it). A resumed session has
        // no wire before its first run, and a switched model has no cache to
        // hit and may reject the old wire. Either way the transcript's
        // thinking blocks are bound to a prefix this request cannot rebuild,
        // so thinking goes off: the API then ignores them instead of
        // rejecting the request.
        let fitted = snapshot
            .as_ref()
            .and_then(|s| s.tools.as_ref())
            .filter(|tools| tools.model == model.spec());
        let (tools, opts) = match fitted {
            Some(SessionTools { wire, .. }) => (Arc::clone(wire), self.request_options()),
            None => (Arc::new(NO_TOOLS), RequestOptions::default()),
        };

        let (tx, rx) = flume::bounded(64);
        self.open_prompt(|app| app.btw_modal.open(&question, rx));
        let request = BtwRequest {
            opts: opts.clamped(&model),
            session_id: SessionRef::from(self.state.session.id),
            provider,
            model,
            system,
            tools,
            messages,
        };
        tokio::spawn(request.run(tx));
    }
}

struct BtwRequest {
    provider: Arc<dyn Provider>,
    model: Model,
    system: String,
    tools: Arc<Value>,
    messages: Vec<Message>,
    opts: RequestOptions,
    session_id: SessionRef,
}

impl BtwRequest {
    async fn run(self, btw_tx: Sender<BtwEvent>) {
        let (event_tx, event_rx) = flume::unbounded();
        let messages = maki_providers::adapt_images_for_model(&self.model, &self.messages).await;

        // The pinned future outlives the stream, so the sender is dropped by
        // hand or `forward` never sees the channel close.
        let stream = async {
            let result = self
                .provider
                .stream_message(
                    &self.model,
                    &messages,
                    &self.system,
                    &self.tools,
                    &event_tx,
                    self.opts,
                    Some(&self.session_id),
                )
                .await;
            drop(event_tx);
            result
        };

        // False when it stopped early, on a tool call or a closed modal, so the
        // stream's outcome never reaches the modal.
        let forward = async {
            while let Ok(event) = event_rx.recv_async().await {
                let event = match event {
                    ProviderEvent::TextDelta { text } => BtwEvent::TextDelta(text),
                    ProviderEvent::ThinkingDelta { text } => BtwEvent::ThinkingDelta(text),
                    ProviderEvent::ToolUseStart { name, .. } => {
                        let _ = btw_tx.send(BtwEvent::Error(reached_for_tool(&name)));
                        return false;
                    }
                    ProviderEvent::PromptProgress { .. } => continue,
                };
                if btw_tx.send(event).is_err() {
                    return false;
                }
            }
            true
        };

        // The channel only closes once `stream` is done, so `forward` ending
        // first means it stopped early, and returning cancels the request
        // instead of streaming for nobody.
        tokio::pin!(stream, forward);
        let result = tokio::select! {
            biased;
            result = &mut stream => result,
            _ = &mut forward => return,
        };
        if forward.await {
            let _ = btw_tx.send(match result {
                Ok(_) => BtwEvent::Done,
                Err(e) => BtwEvent::Error(e.to_string()),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::test_app;
    use crate::components::test_model;
    use arc_swap::ArcSwap;
    use maki_agent::HistorySnapshot;
    use maki_agent::prompt::{PromptId, Slot};
    use maki_agent::tools::ToolRegistry;
    use maki_lua::PluginHost;
    use maki_providers::provider::BoxFuture;
    use maki_providers::{
        AgentError, Effort, ModelInfo, StopReason, StreamResponse, ThinkingConfig, TokenUsage,
    };
    use maki_storage::frame::{PromptFacts, StoredFrame};
    use serde_json::json;
    use test_case::test_case;

    const Q: &str = "why sqlite?";
    const FRAME_POLICY: &str = "Use the active conversation policy.";
    const ANSWER: &str = "Let me check.";
    const OTHER_MODEL: &str = "anthropic/other-model";

    struct Recorded {
        system: String,
        tools: Value,
        opts: RequestOptions,
    }

    struct FakeProvider {
        sent: Sender<Recorded>,
        script: Vec<ProviderEvent>,
    }

    impl Provider for FakeProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            system: &'a str,
            tools: &'a Value,
            events: &'a Sender<ProviderEvent>,
            opts: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                let _ = self.sent.send(Recorded {
                    system: system.to_owned(),
                    tools: tools.clone(),
                    opts,
                });
                for event in self.script.iter().cloned() {
                    events.send(event).unwrap();
                }
                Ok(StreamResponse {
                    message: Message::default(),
                    usage: TokenUsage::default(),
                    stop_reason: Some(StopReason::EndTurn),
                    input_transformations: Vec::new(),
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn rendered_frame() -> StoredFrame {
        StoredFrame::new(FRAME_POLICY.into(), String::new(), None, Vec::new())
    }

    fn wire() -> Value {
        json!([{ "name": "read" }])
    }

    fn share_history(app: &mut App, frame: StoredFrame, fitted_for: Option<String>) {
        let mut snapshot = HistorySnapshot::new(Vec::new());
        snapshot.frame = Some(Arc::new(frame));
        snapshot.tools = fitted_for.map(|model| SessionTools {
            model,
            wire: Arc::new(wire()),
        });
        app.shared_history = Some(Arc::new(ArcSwap::from_pointee(snapshot)));
    }

    async fn ask(app: &mut App) -> Recorded {
        let (sent, requests) = flume::unbounded();
        let provider = FakeProvider {
            sent,
            script: Vec::new(),
        };
        app.start_btw(Q.into(), Arc::new(provider), test_model());
        requests.recv_async().await.unwrap()
    }

    /// An imported session holds a frame with facts and no prompt until its
    /// first run, and an empty system is a 400 on Anthropic.
    #[test_case(None, BTW_FALLBACK_SYSTEM ; "native_fallback")]
    #[test_case(Some(rendered_frame()), FRAME_POLICY ; "stored_frame_precedence")]
    #[test_case(Some(StoredFrame::facts_only(PromptFacts::default())), BTW_FALLBACK_SYSTEM ; "imported_before_first_run")]
    #[tokio::test]
    async fn side_question_uses_the_selected_policy(frame: Option<StoredFrame>, expected: &str) {
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
        let mut app = test_app();
        app.lua_event_handle = host.event_handle();
        if let Some(frame) = frame {
            share_history(&mut app, frame, None);
        }
        assert_eq!(ask(&mut app).await.system, expected);
        let prompts = host.event_handle().collect_prompt_slots();
        assert_eq!(
            prompts.get(PromptId::System, Slot::Identity)[0].content,
            "1"
        );
    }

    #[test_case(Some(test_model().spec()), wire(), ThinkingConfig::Effort(Effort::High) ; "live_wire_shares_the_cache_prefix")]
    #[test_case(Some(OTHER_MODEL.into()), NO_TOOLS, ThinkingConfig::Off ; "switched_model_takes_the_no_tools_path")]
    #[test_case(None, NO_TOOLS, ThinkingConfig::Off ; "no_live_wire_leaves_thinking_off")]
    #[tokio::test]
    async fn side_question_repeats_the_sessions_tools_and_thinking(
        fitted_for: Option<String>,
        tools: Value,
        thinking: ThinkingConfig,
    ) {
        let mut app = test_app();
        app.state.thinking = ThinkingConfig::Effort(Effort::High);
        share_history(&mut app, rendered_frame(), fitted_for);
        let sent = ask(&mut app).await;
        assert_eq!(sent.tools, tools);
        assert_eq!(sent.opts.thinking, thinking);
    }

    /// `Done` once never came: the provider's sender outlived the stream, so
    /// the modal spun forever.
    #[test_case(None, BtwEvent::Done ; "stream_end_is_done")]
    #[test_case(Some("read"), BtwEvent::Error(reached_for_tool("read")) ; "tool_call_is_a_note")]
    #[tokio::test]
    async fn forwards_every_delta_then_stops_the_modal(tool: Option<&str>, last: BtwEvent) {
        let (tx, rx) = flume::bounded(64);
        let mut script = vec![ProviderEvent::TextDelta {
            text: ANSWER.into(),
        }];
        script.extend(tool.map(|name| ProviderEvent::ToolUseStart {
            id: "t1".into(),
            name: name.into(),
        }));
        let provider = FakeProvider {
            sent: flume::unbounded().0,
            script,
        };
        BtwRequest {
            provider: Arc::new(provider),
            model: test_model(),
            system: FRAME_POLICY.into(),
            tools: Arc::new(NO_TOOLS),
            messages: vec![btw_question(Q)],
            opts: RequestOptions::default(),
            session_id: SessionRef::generate(),
        }
        .run(tx)
        .await;
        let events: Vec<BtwEvent> = rx.drain().collect();
        assert_eq!(events, [BtwEvent::TextDelta(ANSWER.into()), last]);
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
