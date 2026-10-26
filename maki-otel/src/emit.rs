//! The maki-facing API. Every function is a no-op when telemetry is off, and
//! nothing here ever returns an error: telemetry must not change what the
//! agent does.

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::logs::LogRecord;

use crate::logs::{
    EVENT_API_ERROR, EVENT_API_REQUEST, EVENT_TOOL_DECISION, EVENT_TOOL_RESULT, EVENT_USER_PROMPT,
};
use crate::{handle, saturating_i64, shared};

pub const START_FRESH: &str = "fresh";
pub const START_RESUME: &str = "resume";
pub const START_CONTINUE: &str = "continue";

pub const TOKEN_INPUT: &str = "input";
pub const TOKEN_OUTPUT: &str = "output";
pub const TOKEN_CACHE_READ: &str = "cacheRead";
pub const TOKEN_CACHE_CREATION: &str = "cacheCreation";

pub const LINES_ADDED: &str = "added";
pub const LINES_REMOVED: &str = "removed";

pub const DECISION_ACCEPT: &str = "accept";
pub const DECISION_REJECT: &str = "reject";

pub const ACTIVE_TIME_CLI: &str = "cli";

const KEY_TYPE: &str = "type";
const KEY_START_TYPE: &str = "start_type";
const KEY_MODEL: &str = "model";
const KEY_PROVIDER: &str = "provider";
const KEY_TOOL_NAME: &str = "tool_name";
const KEY_TOOL_SOURCE: &str = "tool_source";
const KEY_DECISION: &str = "decision";
const KEY_SOURCE: &str = "source";
const KEY_SUCCESS: &str = "success";
const KEY_DURATION_MS: &str = "duration_ms";
const KEY_ERROR: &str = "error";
const KEY_ERROR_TYPE: &str = "error_type";
const KEY_STATUS_CODE: &str = "status_code";
const KEY_ATTEMPT: &str = "attempt";
const KEY_STOP_REASON: &str = "stop_reason";
const KEY_INPUT_TOKENS: &str = "input_tokens";
const KEY_OUTPUT_TOKENS: &str = "output_tokens";
const KEY_CACHE_READ_TOKENS: &str = "cache_read_tokens";
const KEY_CACHE_CREATION_TOKENS: &str = "cache_creation_tokens";
const KEY_COST_USD: &str = "cost_usd";
const KEY_PROMPT: &str = "prompt";
const KEY_PROMPT_LENGTH: &str = "prompt_length";
const KEY_TOOL_INPUT: &str = "tool_input";

/// The one entry point for session starts: the id is set before counting, so
/// a counted session can never miss it.
pub fn session_started(start_type: &'static str, session_id: Option<&str>) {
    if let Some(id) = session_id {
        crate::set_session_id(id);
    }
    let Some(handle) = handle() else {
        return;
    };
    handle.count(
        |m| &m.session_count,
        1,
        &[KeyValue::new(KEY_START_TYPE, start_type)],
    );
}

pub fn user_prompt(prompt: &str) {
    let Some(handle) = handle() else {
        return;
    };
    handle.event(EVENT_USER_PROMPT, |record| {
        record.add_attribute(KEY_PROMPT_LENGTH, saturating_i64(prompt.chars().count()));
        if handle.log_user_prompts {
            record.add_attribute(KEY_PROMPT, handle.truncate(prompt));
        }
    });
}

pub struct ApiRequest<'a> {
    pub model: &'a str,
    pub provider: &'a str,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_usd: f64,
    pub duration: Duration,
    pub stop_reason: Option<&'a str>,
}

pub fn api_request(request: &ApiRequest<'_>) {
    let Some(handle) = handle() else {
        return;
    };
    let model = shared(request.model);
    let provider = shared(request.provider);
    handle.event(EVENT_API_REQUEST, |record| {
        record.add_attribute(KEY_MODEL, model.clone());
        record.add_attribute(KEY_PROVIDER, provider.clone());
        record.add_attribute(KEY_INPUT_TOKENS, saturating_i64(request.input_tokens));
        record.add_attribute(KEY_OUTPUT_TOKENS, saturating_i64(request.output_tokens));
        record.add_attribute(
            KEY_CACHE_READ_TOKENS,
            saturating_i64(request.cache_read_tokens),
        );
        record.add_attribute(
            KEY_CACHE_CREATION_TOKENS,
            saturating_i64(request.cache_creation_tokens),
        );
        record.add_attribute(KEY_COST_USD, request.cost_usd);
        record.add_attribute(
            KEY_DURATION_MS,
            saturating_i64(request.duration.as_millis()),
        );
        if let Some(stop_reason) = request.stop_reason {
            record.add_attribute(KEY_STOP_REASON, stop_reason.to_string());
        }
    });

    let model_attrs = [
        KeyValue::new(KEY_MODEL, model),
        KeyValue::new(KEY_PROVIDER, provider),
    ];
    for (kind, count) in [
        (TOKEN_INPUT, request.input_tokens),
        (TOKEN_OUTPUT, request.output_tokens),
        (TOKEN_CACHE_READ, request.cache_read_tokens),
        (TOKEN_CACHE_CREATION, request.cache_creation_tokens),
    ] {
        if count == 0 {
            continue;
        }
        let [model, provider] = model_attrs.clone();
        handle.count(
            |m| &m.token_usage,
            count,
            &[model, provider, KeyValue::new(KEY_TYPE, kind)],
        );
    }
    if request.cost_usd > 0.0 {
        handle.count(|m| &m.cost_usage, request.cost_usd, &model_attrs);
    }
}

pub struct ApiError<'a> {
    pub model: &'a str,
    pub provider: &'a str,
    pub error: &'a str,
    pub status_code: Option<u16>,
    pub attempt: u32,
    pub duration: Duration,
}

pub fn api_error(error: &ApiError<'_>) {
    let Some(handle) = handle() else {
        return;
    };
    handle.event(EVENT_API_ERROR, |record| {
        record.add_attribute(KEY_MODEL, error.model.to_string());
        record.add_attribute(KEY_PROVIDER, error.provider.to_string());
        record.add_attribute(KEY_ERROR, handle.truncate(error.error));
        if let Some(status_code) = error.status_code {
            record.add_attribute(KEY_STATUS_CODE, status_code);
        }
        record.add_attribute(KEY_ATTEMPT, error.attempt);
        record.add_attribute(KEY_DURATION_MS, saturating_i64(error.duration.as_millis()));
    });
}

pub struct ToolResult<'a> {
    pub tool_name: &'a str,
    pub tool_source: &'a str,
    pub success: bool,
    pub duration: Duration,
    pub error_type: Option<&'a str>,
    pub tool_input: Option<&'a str>,
}

pub fn tool_result(result: &ToolResult<'_>) {
    let Some(handle) = handle() else {
        return;
    };
    handle.event(EVENT_TOOL_RESULT, |record| {
        record.add_attribute(KEY_TOOL_NAME, result.tool_name.to_string());
        record.add_attribute(KEY_TOOL_SOURCE, result.tool_source.to_string());
        record.add_attribute(KEY_SUCCESS, result.success);
        record.add_attribute(KEY_DURATION_MS, saturating_i64(result.duration.as_millis()));
        if let Some(error_type) = result.error_type {
            record.add_attribute(KEY_ERROR_TYPE, error_type.to_string());
        }
        if handle.log_tool_details
            && let Some(input) = result.tool_input
        {
            record.add_attribute(KEY_TOOL_INPUT, handle.truncate(input));
        }
    });
}

/// Both the event and the counter: dashboards want the rate, audits the detail.
pub fn tool_decision(tool_name: &str, decision: &'static str, source: &'static str) {
    let Some(handle) = handle() else {
        return;
    };
    let tool_name = shared(tool_name);
    handle.event(EVENT_TOOL_DECISION, |record| {
        record.add_attribute(KEY_TOOL_NAME, tool_name.clone());
        record.add_attribute(KEY_DECISION, decision);
        record.add_attribute(KEY_SOURCE, source);
    });
    handle.count(
        |m| &m.tool_decision,
        1,
        &[
            KeyValue::new(KEY_TOOL_NAME, tool_name),
            KeyValue::new(KEY_DECISION, decision),
            KeyValue::new(KEY_SOURCE, source),
        ],
    );
}

pub fn lines_of_code(added: u64, removed: u64) {
    let Some(handle) = handle() else {
        return;
    };
    for (kind, count) in [(LINES_ADDED, added), (LINES_REMOVED, removed)] {
        if count == 0 {
            continue;
        }
        handle.count(
            |m| &m.lines_of_code,
            count,
            &[KeyValue::new(KEY_TYPE, kind)],
        );
    }
}

pub fn commit_created() {
    if let Some(handle) = handle() {
        handle.count(|m| &m.commit_count, 1, &[]);
    }
}

pub fn pull_request_created() {
    if let Some(handle) = handle() {
        handle.count(|m| &m.pull_request_count, 1, &[]);
    }
}

/// Time the agent spent working, as opposed to waiting for the user.
pub fn active_time(duration: Duration) {
    if let Some(handle) = handle() {
        handle.count(
            |m| &m.active_time,
            duration.as_secs_f64(),
            &[KeyValue::new(KEY_TYPE, ACTIVE_TIME_CLI)],
        );
    }
}
