//! Maps the `question` tool onto ACP form elicitation (`elicitation/create`).
//! The tool keeps its Lua-advertised schema; only execution is rerouted here,
//! so Zed and friends render native forms instead of a spinner nobody can answer.

use std::collections::BTreeMap;

use agent_client_protocol_schema::{
    ClientCapabilities, CreateElicitationRequest, CreateElicitationResponse, ElicitationAction,
    ElicitationContentValue, ElicitationFormMode, ElicitationPropertySchema, ElicitationSchema,
    ElicitationSessionScope, EnumOption, MultiSelectPropertySchema, SessionId,
    StringPropertySchema, ToolCallId,
};
use serde::Deserialize;
use serde_json::Value;

/// Mirrors the Lua question tool's dismiss output so the model sees the same
/// text regardless of frontend.
pub const DISMISSED: &str = "(question dismissed by user)";
const NO_ANSWER: &str = "(no answer)";

#[derive(Deserialize)]
struct Question {
    question: String,
    #[serde(default)]
    header: String,
    #[serde(default)]
    options: Vec<QuestionOption>,
    #[serde(default, rename = "multiSelect", alias = "multiple")]
    multi_select: bool,
}

#[derive(Deserialize)]
struct QuestionOption {
    label: String,
    #[serde(default)]
    description: String,
}

pub fn supports_form(caps: &ClientCapabilities) -> bool {
    caps.elicitation.as_ref().is_some_and(|e| e.form.is_some())
}

fn parse_questions(input: &Value) -> Result<Vec<Question>, String> {
    let questions = input.get("questions").cloned().unwrap_or(Value::Null);
    serde_json::from_value(questions).map_err(|e| format!("invalid questions: {e}"))
}

fn enum_options(options: &[QuestionOption]) -> Vec<EnumOption> {
    options
        .iter()
        .map(|opt| {
            let title = if opt.description.is_empty() {
                opt.label.clone()
            } else {
                format!("{} - {}", opt.label, opt.description)
            };
            EnumOption::new(opt.label.clone(), title)
        })
        .collect()
}

fn property(q: &Question) -> ElicitationPropertySchema {
    let title = q.question.clone();
    if q.options.is_empty() {
        ElicitationPropertySchema::String(StringPropertySchema::new().title(title))
    } else if q.multi_select {
        ElicitationPropertySchema::Array(
            MultiSelectPropertySchema::titled(enum_options(&q.options)).title(title),
        )
    } else {
        ElicitationPropertySchema::String(
            StringPropertySchema::new()
                .title(title)
                .one_of(enum_options(&q.options)),
        )
    }
}

/// Property keys are positional (`q1`, `q2`, ...) so answers map back to
/// questions even when headers repeat or are missing.
fn key(index: usize) -> String {
    format!("q{}", index + 1)
}

pub fn form_request(
    session_id: &str,
    tool_call_id: Option<String>,
    input: &Value,
) -> Result<CreateElicitationRequest, String> {
    let questions = parse_questions(input)?;
    if questions.is_empty() {
        return Err("at least one question is required".to_owned());
    }

    let mut schema = ElicitationSchema::new();
    schema.properties = questions
        .iter()
        .enumerate()
        .map(|(i, q)| (key(i), property(q)))
        .collect();

    let scope = ElicitationSessionScope::new(SessionId::from(session_id.to_owned()))
        .tool_call_id(tool_call_id.map(ToolCallId::from));
    let message = match questions.as_slice() {
        [only] => only.question.clone(),
        many => format!("{} questions", many.len()),
    };
    Ok(CreateElicitationRequest::new(
        ElicitationFormMode::new(scope, schema),
        message,
    ))
}

fn answer_text(value: Option<&ElicitationContentValue>) -> Option<String> {
    match value? {
        ElicitationContentValue::String(s) if !s.is_empty() => Some(s.clone()),
        ElicitationContentValue::StringArray(items) if !items.is_empty() => Some(items.join(", ")),
        ElicitationContentValue::Integer(n) => Some(n.to_string()),
        ElicitationContentValue::Number(n) => Some(n.to_string()),
        ElicitationContentValue::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Turns the client's `elicitation/create` result into the same markdown the
/// Lua tool feeds the model: one `header: labels` line per question.
pub fn format_response(input: &Value, raw_result: &str) -> String {
    let Ok(response) = serde_json::from_str::<CreateElicitationResponse>(raw_result) else {
        return DISMISSED.to_owned();
    };
    let ElicitationAction::Accept(accept) = response.action else {
        return DISMISSED.to_owned();
    };
    let content: BTreeMap<String, ElicitationContentValue> = accept.content.unwrap_or_default();
    let questions = parse_questions(input).unwrap_or_default();

    questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let label = if q.header.is_empty() {
                format!("Q{}", i + 1)
            } else {
                q.header.clone()
            };
            let answer = answer_text(content.get(&key(i))).unwrap_or_else(|| NO_ANSWER.to_owned());
            format!("{label}: {answer}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::ElicitationScope;
    use test_case::test_case;

    use super::*;

    fn questions_input() -> Value {
        serde_json::json!({
            "questions": [
                {
                    "question": "Pick a framework",
                    "header": "Framework",
                    "options": [
                        { "label": "axum", "description": "tokio based" },
                        { "label": "actix" }
                    ]
                },
                {
                    "question": "Which features?",
                    "header": "Features",
                    "multiSelect": true,
                    "options": [{ "label": "auth" }, { "label": "uploads" }]
                },
                { "question": "Anything else?" }
            ]
        })
    }

    #[test]
    fn form_request_maps_questions_to_schema() {
        let req = form_request("sess_1", Some("tool_1".to_owned()), &questions_input()).unwrap();
        assert_eq!(req.message, "3 questions");

        let ElicitationScope::Session(scope) = req.scope() else {
            panic!("expected session scope");
        };
        assert_eq!(scope.session_id.0.as_ref(), "sess_1");
        assert_eq!(
            scope.tool_call_id.as_ref().map(|t| t.0.as_ref()),
            Some("tool_1")
        );

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["mode"], "form");
        let props = &json["requestedSchema"]["properties"];
        assert_eq!(props["q1"]["type"], "string");
        assert_eq!(props["q1"]["oneOf"][0]["const"], "axum");
        assert_eq!(props["q2"]["type"], "array");
        assert_eq!(props["q3"]["type"], "string");
        assert!(props["q3"].get("oneOf").is_none());
    }

    #[test]
    fn single_question_is_the_message() {
        let input = serde_json::json!({ "questions": [{ "question": "Proceed?" }] });
        let req = form_request("sess_1", None, &input).unwrap();
        assert_eq!(req.message, "Proceed?");
    }

    #[test_case(serde_json::json!({}) ; "missing_questions")]
    #[test_case(serde_json::json!({ "questions": [] }) ; "empty_questions")]
    fn form_request_rejects_bad_input(input: Value) {
        assert!(form_request("sess_1", None, &input).is_err());
    }

    #[test]
    fn accept_response_formats_answers() {
        let raw = serde_json::json!({
            "action": "accept",
            "content": { "q1": "axum", "q2": ["auth", "uploads"] }
        })
        .to_string();
        assert_eq!(
            format_response(&questions_input(), &raw),
            "Framework: axum\nFeatures: auth, uploads\nQ3: (no answer)"
        );
    }

    #[test_case(r#"{"action":"decline"}"# ; "decline")]
    #[test_case(r#"{"action":"cancel"}"# ; "cancel")]
    #[test_case("not json" ; "unparsable")]
    #[test_case("null" ; "jsonrpc_error_forwarded_as_null")]
    fn non_accept_is_dismissed(raw: &str) {
        assert_eq!(format_response(&questions_input(), raw), DISMISSED);
    }

    #[test]
    fn supports_form_requires_form_capability() {
        assert!(!supports_form(&ClientCapabilities::default()));
        let caps: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "elicitation": { "form": {} }
        }))
        .unwrap();
        assert!(supports_form(&caps));
        let url_only: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "elicitation": { "url": {} }
        }))
        .unwrap();
        assert!(!supports_form(&url_only));
    }
}
