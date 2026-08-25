//! The maki-facing API. Every function is a no-op when telemetry is off, and
//! nothing here ever returns an error: telemetry must not change what the
//! agent does.

use std::time::Duration;

use crate::attr::AttrSet;
use crate::handle;
use crate::logs::{
    EVENT_API_ERROR, EVENT_API_REQUEST, EVENT_TOOL_DECISION, EVENT_TOOL_RESULT, EVENT_USER_PROMPT,
};
use crate::metrics::{
    ACTIVE_TIME, COMMIT_COUNT, COST_USAGE, LINES_OF_CODE, PULL_REQUEST_COUNT, SESSION_COUNT,
    TOKEN_USAGE, TOOL_DECISION, Value,
};

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
    handle.record(
        &SESSION_COUNT,
        Value::Int(1),
        AttrSet::new().with(KEY_START_TYPE, start_type),
    );
}

pub fn user_prompt(prompt: &str) {
    let Some(handle) = handle() else {
        return;
    };
    let mut attrs = AttrSet::new().with(KEY_PROMPT_LENGTH, prompt.chars().count());
    if handle.log_user_prompts {
        attrs.insert(KEY_PROMPT, handle.truncate(prompt));
    }
    handle.event(EVENT_USER_PROMPT, attrs);
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
    let model_attrs = AttrSet::new()
        .with(KEY_MODEL, request.model)
        .with(KEY_PROVIDER, request.provider);
    handle.event(
        EVENT_API_REQUEST,
        model_attrs
            .clone()
            .with(KEY_INPUT_TOKENS, request.input_tokens)
            .with(KEY_OUTPUT_TOKENS, request.output_tokens)
            .with(KEY_CACHE_READ_TOKENS, request.cache_read_tokens)
            .with(KEY_CACHE_CREATION_TOKENS, request.cache_creation_tokens)
            .with(KEY_COST_USD, request.cost_usd)
            .with(KEY_DURATION_MS, request.duration.as_millis() as u64)
            .with_opt(KEY_STOP_REASON, request.stop_reason),
    );

    for (kind, count) in [
        (TOKEN_INPUT, request.input_tokens),
        (TOKEN_OUTPUT, request.output_tokens),
        (TOKEN_CACHE_READ, request.cache_read_tokens),
        (TOKEN_CACHE_CREATION, request.cache_creation_tokens),
    ] {
        if count == 0 {
            continue;
        }
        handle.record(
            &TOKEN_USAGE,
            Value::Int(count as i64),
            model_attrs.clone().with(KEY_TYPE, kind),
        );
    }
    if request.cost_usd > 0.0 {
        handle.record(&COST_USAGE, Value::Double(request.cost_usd), model_attrs);
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
    handle.event(
        EVENT_API_ERROR,
        AttrSet::new()
            .with(KEY_MODEL, error.model)
            .with(KEY_PROVIDER, error.provider)
            .with(KEY_ERROR, handle.truncate(error.error))
            .with_opt(KEY_STATUS_CODE, error.status_code.map(i64::from))
            .with(KEY_ATTEMPT, i64::from(error.attempt))
            .with(KEY_DURATION_MS, error.duration.as_millis() as u64),
    );
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
    let mut attrs = AttrSet::new()
        .with(KEY_TOOL_NAME, result.tool_name)
        .with(KEY_TOOL_SOURCE, result.tool_source)
        .with(KEY_SUCCESS, result.success)
        .with(KEY_DURATION_MS, result.duration.as_millis() as u64)
        .with_opt(KEY_ERROR_TYPE, result.error_type);
    if handle.log_tool_details
        && let Some(input) = result.tool_input
    {
        attrs.insert(KEY_TOOL_INPUT, handle.truncate(input));
    }
    handle.event(EVENT_TOOL_RESULT, attrs);
}

/// Both the event and the counter: dashboards want the rate, audits the detail.
pub fn tool_decision(tool_name: &str, decision: &'static str, source: &'static str) {
    let Some(handle) = handle() else {
        return;
    };
    let attrs = AttrSet::new()
        .with(KEY_TOOL_NAME, tool_name)
        .with(KEY_DECISION, decision)
        .with(KEY_SOURCE, source);
    handle.event(EVENT_TOOL_DECISION, attrs.clone());
    handle.record(&TOOL_DECISION, Value::Int(1), attrs);
}

pub fn lines_of_code(added: u64, removed: u64) {
    let Some(handle) = handle() else {
        return;
    };
    for (kind, count) in [(LINES_ADDED, added), (LINES_REMOVED, removed)] {
        if count == 0 {
            continue;
        }
        handle.record(
            &LINES_OF_CODE,
            Value::Int(count as i64),
            AttrSet::new().with(KEY_TYPE, kind),
        );
    }
}

pub fn commit_created() {
    if let Some(handle) = handle() {
        handle.record(&COMMIT_COUNT, Value::Int(1), AttrSet::new());
    }
}

pub fn pull_request_created() {
    if let Some(handle) = handle() {
        handle.record(&PULL_REQUEST_COUNT, Value::Int(1), AttrSet::new());
    }
}

/// Time the agent spent working, as opposed to waiting for the user.
pub fn active_time(duration: Duration) {
    if let Some(handle) = handle() {
        handle.record(
            &ACTIVE_TIME,
            Value::Double(duration.as_secs_f64()),
            AttrSet::new().with(KEY_TYPE, ACTIVE_TIME_CLI),
        );
    }
}
