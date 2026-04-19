use crate::{AgentEvent, QuestionAnswer, QuestionInfo, ToolOutput};
use maki_tool_macro::Tool;
use serde::Deserialize;

const EMPTY_QUESTIONS: &str = "at least one question is required";
const CHANNEL_CLOSED: &str = "question cancelled: response channel closed";

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Question {
    #[param(description = "List of questions to ask the user")]
    questions: Vec<QuestionInfo>,
}

impl Question {
    pub const NAME: &str = "question";
    pub const DESCRIPTION: &str = include_str!("question.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[{"questions": [{"question": "Which database should we use?", "options": [{"label": "PostgreSQL"}, {"label": "SQLite"}]}]}]"#,
    );

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        if self.questions.is_empty() {
            return Err(EMPTY_QUESTIONS.into());
        }

        let (Some(tool_use_id), Some(rx)) =
            (ctx.tool_use_id.as_deref(), ctx.user_response_rx.as_ref())
        else {
            return Ok(ToolOutput::Plain(self.format_questions()));
        };

        ctx.event_tx.try_send(AgentEvent::QuestionPrompt {
            id: tool_use_id.to_owned(),
            questions: self.questions.clone(),
        });

        let rx = rx.lock().await;
        let recv = async {
            match rx.recv_async().await {
                Ok(answer) => Ok(Self::format_answer(&self.questions, &answer)),
                Err(_) => Err(CHANNEL_CLOSED.into()),
            }
        };
        ctx.cancel.race(recv).await?
    }

    pub fn start_summary(&self) -> String {
        let n = self.questions.len();
        format!("{n} question{}", if n == 1 { "" } else { "s" })
    }
}

super::impl_tool!(Question, audience = super::ToolAudience::MAIN);

impl super::ToolInvocation for Question {
    fn start_summary(&self) -> super::SummaryFuture {
        super::SummaryFuture::Ready(Question::start_summary(self))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Question::execute(&self, ctx).await })
    }
}

impl Question {
    fn format_answer(questions: &[QuestionInfo], raw: &str) -> ToolOutput {
        let Ok(answers) = serde_json::from_str::<Vec<Vec<String>>>(raw) else {
            return ToolOutput::Plain(raw.to_string());
        };
        let pairs = questions
            .iter()
            .zip(answers.iter())
            .map(|(q, a)| QuestionAnswer {
                question: q.question.clone(),
                answer: a.join(", "),
            })
            .collect();
        ToolOutput::QuestionAnswers(pairs)
    }

    fn format_questions(&self) -> String {
        self.questions
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let mut line = format!("{}. {}", i + 1, q.question);
                for opt in &q.options {
                    line.push_str(&format!("\n   - {}", opt.label));
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_lock::Mutex;
    use serde_json::json;

    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::{stub_ctx, stub_ctx_with};

    const SINGLE_Q: &str = r#"{"questions": [{"question": "Preferred DB?"}]}"#;

    fn qi(question: &str) -> QuestionInfo {
        QuestionInfo {
            question: question.into(),
            header: String::new(),
            options: vec![],
            multiple: false,
        }
    }

    fn q_with_options() -> serde_json::Value {
        json!({"questions": [{
            "question": "Pick a DB",
            "header": "DB",
            "options": [
                {"label": "PostgreSQL", "description": "Relational"},
                {"label": "Redis", "description": "Key-value"}
            ]
        }]})
    }

    #[test]
    fn empty_questions_returns_error() {
        smol::block_on(async {
            let q = Question::parse_input(&json!({"questions": []})).unwrap();
            let err = q.execute(&stub_ctx(&AgentMode::Build)).await.unwrap_err();
            assert_eq!(err, EMPTY_QUESTIONS);
        });
    }

    #[test]
    fn formats_questions_with_options_without_channel() {
        smol::block_on(async {
            let q = Question::parse_input(&q_with_options()).unwrap();
            let output = q.execute(&stub_ctx(&AgentMode::Build)).await.unwrap();
            let text = output.as_text();
            assert!(text.contains("Pick a DB"));
            assert!(text.contains("- PostgreSQL"));
            assert!(text.contains("- Redis"));
        });
    }

    #[test]
    fn blocks_on_channel_and_returns_structured_answer() {
        smol::block_on(async {
            let (raw_tx, event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(raw_tx, 0);
            let (answer_tx, answer_rx) = flume::unbounded();
            let answer_mutex = Mutex::new(answer_rx);
            let mode = AgentMode::Build;
            let mut ctx = stub_ctx_with(&mode, Some(&event_tx), Some("q1"));
            ctx.user_response_rx = Some(Arc::new(answer_mutex));

            let input: serde_json::Value = serde_json::from_str(SINGLE_Q).unwrap();
            let q = Question::parse_input(&input).unwrap();

            let handle = smol::spawn({
                let ctx = ctx.clone();
                let q = q.clone();
                async move { q.execute(&ctx).await }
            });

            let prompt_event = event_rx.recv_async().await.unwrap();
            assert!(matches!(
                prompt_event.event,
                AgentEvent::QuestionPrompt { ref id, ref questions }
                if id == "q1" && questions[0].question == "Preferred DB?"
            ));

            answer_tx.try_send(r#"[["PostgreSQL"]]"#.into()).unwrap();
            let output = handle.await.unwrap();
            match output {
                ToolOutput::QuestionAnswers(pairs) => {
                    assert_eq!(pairs.len(), 1);
                    assert_eq!(pairs[0].question, "Preferred DB?");
                    assert_eq!(pairs[0].answer, "PostgreSQL");
                }
                other => panic!("expected QuestionAnswers, got {other:?}"),
            }
        });
    }

    #[test]
    fn channel_closed_returns_error() {
        smol::block_on(async {
            let (raw_tx, _event_rx) = flume::unbounded();
            let event_tx = crate::EventSender::new(raw_tx, 0);
            let (_, answer_rx) = flume::unbounded::<String>();
            let answer_mutex = Mutex::new(answer_rx);
            let mode = AgentMode::Build;
            let mut ctx = stub_ctx_with(&mode, Some(&event_tx), Some("q2"));
            ctx.user_response_rx = Some(Arc::new(answer_mutex));

            let input: serde_json::Value = serde_json::from_str(SINGLE_Q).unwrap();
            let q = Question::parse_input(&input).unwrap();
            let err = q.execute(&ctx).await.unwrap_err();
            assert_eq!(err, CHANNEL_CLOSED);
        });
    }

    #[test]
    fn format_answer_non_json_passed_through() {
        let questions = vec![qi("Q?")];
        assert!(matches!(
            Question::format_answer(&questions, "plain text"),
            ToolOutput::Plain(ref s) if s == "plain text"
        ));
    }

    #[test]
    fn format_answer_multi_question_multi_select() {
        let questions = vec![qi("Language?"), qi("Framework?")];
        let raw = r#"[["Rust"],["Axum","Actix"]]"#;
        let result = Question::format_answer(&questions, raw);
        let expected_pairs = vec![
            QuestionAnswer {
                question: "Language?".into(),
                answer: "Rust".into(),
            },
            QuestionAnswer {
                question: "Framework?".into(),
                answer: "Axum, Actix".into(),
            },
        ];
        match result {
            ToolOutput::QuestionAnswers(pairs) => assert_eq!(pairs, expected_pairs),
            other => panic!("expected QuestionAnswers, got {other:?}"),
        }
    }
}
