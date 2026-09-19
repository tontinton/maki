use maki_config::providers::Protocol;

use crate::model::ModelFamily;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, ProviderSpec,
};

const SLUG: &str = "regolo";
const DISPLAY_NAME: &str = "Regolo";
const ENV_VAR: &str = "REGOLO_API_KEY";
const BASE_URL: &str = "https://api.regolo.ai/v1";
const DEFAULT_MODEL: &str = "regolo/qwen3-coder-next";
const LOGIN_URL: &str = "https://dashboard.regolo.ai";
const FEATURES: &str = "EU-hosted open-weight models with tool calling. The catalogue and prices are listed live from the API";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(120_000),
    fallback_context_window: 120_000,
    models_toml: include_str!("../../models/regolo.toml"),
    pricing_schedule: None,
    build: Build::Declared,
    aperture: Some(ApertureRoute {
        path_prefix: DEFAULT_PATH_PREFIX,
    }),
    login: Some(LoginConfig {
        protocol: Protocol::Openai,
        default_base_url: BASE_URL,
        default_model: DEFAULT_MODEL,
        plans: None,
        login_url: Some(LOGIN_URL),
        needs_url: false,
    }),
    docs: GeneratedDocs {
        api_urls: &[BASE_URL],
        features: Some(FEATURES),
        auth: AuthDoc::EnvVar,
        catalog: CatalogDoc::Table,
        trailing_notes: &[],
    },
};

inventory::submit!(SPEC.config_row());

/// The recorded cases the bundled `regolo` plugin replays.
///
/// Loopback serves `<origin>/v1`, so the chat and `/models` requests land under
/// `/v1` and the management endpoints at the root, which the plugin reaches by
/// stripping the version segment. Every script that reaches more than one
/// endpoint is routed by path: the usage side calls may race, and a routed
/// script keeps the observation free of arrival order.
#[cfg(any(test, feature = "test-support"))]
pub mod fixtures {
    use crate::model::Model;
    use crate::providers::replay::Fixture;
    use crate::test_support::Canned;
    use crate::{Effort, ThinkingConfig};

    /// Spelled out instead of reusing `DEFAULT_MODEL`, so a new default can
    /// never move the goldens, which are not recorded again.
    const MODEL_SPEC: &str = "regolo/qwen3-coder-next";
    /// Above what the `standard` dialect accepts, so the wire shows it snapped
    /// to `high` rather than passed through.
    const EFFORT: Effort = Effort::XHigh;
    const UNKNOWN_MODEL: &str = "the curated table has no such model";

    const MODELS_PATH: &str = "/v1/models";
    const MODEL_GROUP_INFO_PATH: &str = "/model_group/info";
    const KEY_INFO_PATH: &str = "/key/info";
    const ACTIVITY_PATH: &str = "/global/activity";
    const SPEND_LOGS_PATH: &str = "/spend/logs/v2";

    const UNAUTHORIZED_BODY: &str =
        r#"{"error":{"message":"Authentication Error, Invalid proxy server token passed."}}"#;
    const SERVER_ERROR_BODY: &str = r#"{"error":{"message":"internal server error"}}"#;

    const SUCCESS_TRANSCRIPT: &str = r#"data: {"choices":[{"delta":{"reasoning_content":"weighing the options"}}]}

data: {"choices":[{"delta":{"content":"Hello"}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17,"prompt_tokens_details":{"cached_tokens":4}}}

data: [DONE]

"#;

    const SUCCESS_SCRIPT: &[Canned] = &[Canned::sse(SUCCESS_TRANSCRIPT)];

    pub const SUCCESS: Fixture = Fixture {
        name: "success",
        script: SUCCESS_SCRIPT,
        thinking: ThinkingConfig::Effort(EFFORT),
        session: None,
    };
    /// The `standard` dialect has no spelling for off, so nothing reaches the
    /// wire.
    pub const THINKING_OFF: Fixture = Fixture {
        name: "thinking_off",
        script: SUCCESS_SCRIPT,
        thinking: ThinkingConfig::Off,
        session: None,
    };

    /// Unsorted, with a duplicate id, an upper-case id that sorts first
    /// bytewise, an id no group describes, and three entries without a string
    /// id. The per-model fields the compat lister reads are ignored here.
    const MODELS_BODY: &str = r#"{"object":"list","data":[
        {"id":"qwen3.5-9b","object":"model","owned_by":"regolo","context_length":4096},
        {"id":"glm5.2","object":"model"},
        {"id":"Qwen3-Embedding-8B","object":"model"},
        {"id":"qwen3-coder-next","object":"model"},
        {"id":"gpt-oss-120b","object":"model"},
        {"id":"glm5.2","object":"model"},
        {"id":"llama-huge","object":"model"},
        {"id":"brand-new-model","object":"model"},
        {"id":"legacy-negative","object":"model"},
        {"id":"half-priced","object":"model"},
        {"id":null,"object":"model"},
        {"object":"model"},
        {"id":42,"object":"model"}
    ]}"#;

    /// `glm5.2` is described three times: the later chat group replaces the
    /// first, and the trailing non-chat one is filtered out before it could.
    /// Token limits come as floats, fractions, integers, negatives, nulls and
    /// values past `u32`. `llama-huge` shows an out-of-range input window does
    /// not fall back to `max_tokens`. `gpt-oss-120b` prices at `0.125` per
    /// million, `half-priced` has one price null, and `orphan-chat` has no id.
    const MODEL_GROUPS_BODY: &str = r#"{"data":[
        {"model_group":"glm5.2","mode":"chat","input_cost_per_token":2e-06,"output_cost_per_token":5.2e-06,"max_input_tokens":96000.0,"max_output_tokens":96000.0,"max_tokens":null,"supports_reasoning":true,"supports_vision":false},
        {"model_group":"qwen3-coder-next","mode":"chat","input_cost_per_token":5e-07,"output_cost_per_token":2e-06,"max_input_tokens":null,"max_output_tokens":120000,"max_tokens":240000.0,"supports_reasoning":false,"supports_vision":true,"supports_function_calling":true},
        {"model_group":"qwen3.5-9b","mode":"chat","input_cost_per_token":7e-08,"output_cost_per_token":3.5e-07,"max_input_tokens":80000.7,"max_output_tokens":120000.0,"max_tokens":200000.0,"supports_reasoning":true,"supports_vision":false},
        {"model_group":"gpt-oss-120b","mode":"chat","input_cost_per_token":1.25e-07,"output_cost_per_token":6.25e-07,"max_input_tokens":131072,"max_output_tokens":null,"max_tokens":null,"supports_reasoning":true,"supports_vision":false},
        {"model_group":"llama-huge","mode":"chat","input_cost_per_token":1e-06,"output_cost_per_token":1e-06,"max_input_tokens":5000000000.0,"max_output_tokens":4294967296,"max_tokens":131072.0,"supports_reasoning":false,"supports_vision":false},
        {"model_group":"legacy-negative","mode":"chat","input_cost_per_token":-1e-06,"output_cost_per_token":0.0,"max_input_tokens":-1.0,"max_output_tokens":-512,"max_tokens":null,"supports_reasoning":false,"supports_vision":false},
        {"model_group":"half-priced","mode":"chat","input_cost_per_token":1e-06,"output_cost_per_token":null,"max_input_tokens":null,"max_output_tokens":null,"max_tokens":null,"supports_reasoning":false,"supports_vision":false},
        {"model_group":"Qwen3-Embedding-8B","mode":"embedding","input_cost_per_token":0.0,"output_cost_per_token":0.0,"max_input_tokens":32000.0,"max_output_tokens":null,"max_tokens":null,"supports_reasoning":false,"supports_vision":false},
        {"model_group":"orphan-chat","mode":"chat","input_cost_per_token":1e-06,"output_cost_per_token":1e-06,"max_input_tokens":8192.0,"max_output_tokens":8192.0,"max_tokens":null,"supports_reasoning":false,"supports_vision":false},
        {"model_group":"glm5.2","mode":"chat","input_cost_per_token":2e-06,"output_cost_per_token":5.2e-06,"max_input_tokens":64000.0,"max_output_tokens":32000.0,"max_tokens":null,"supports_reasoning":true,"supports_vision":true},
        {"model_group":"glm5.2","mode":"rerank","input_cost_per_token":null,"output_cost_per_token":null,"max_input_tokens":null,"max_output_tokens":null,"max_tokens":null,"supports_reasoning":false,"supports_vision":false}
    ]}"#;

    /// One price as a string fails the whole response, not the one group.
    const MALFORMED_MODEL_GROUPS_BODY: &str = r#"{"data":[
        {"model_group":"qwen3-coder-next","mode":"chat","input_cost_per_token":5e-07,"output_cost_per_token":2e-06,"max_input_tokens":null,"max_output_tokens":120000.0,"max_tokens":240000.0,"supports_reasoning":false,"supports_vision":true},
        {"model_group":"glm5.2","mode":"chat","input_cost_per_token":"0.000002","output_cost_per_token":5.2e-06,"max_input_tokens":96000.0,"max_output_tokens":96000.0,"max_tokens":null,"supports_reasoning":true,"supports_vision":false}
    ]}"#;

    const MODELS_OK: Canned = Canned::at(MODELS_PATH, Canned::json(200, MODELS_BODY));

    pub const MODELS: Fixture = Fixture {
        name: "models",
        script: &[
            MODELS_OK,
            Canned::at(MODEL_GROUP_INFO_PATH, Canned::json(200, MODEL_GROUPS_BODY)),
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    /// The group endpoint has 500ed in the wild: the live ids stay, bare.
    pub const MODELS_WITHOUT_GROUPS: Fixture = Fixture {
        name: "models_without_groups",
        script: &[
            MODELS_OK,
            Canned::at(MODEL_GROUP_INFO_PATH, Canned::json(500, SERVER_ERROR_BODY)),
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    pub const MODELS_MALFORMED_GROUPS: Fixture = Fixture {
        name: "models_malformed_groups",
        script: &[
            MODELS_OK,
            Canned::at(
                MODEL_GROUP_INFO_PATH,
                Canned::json(200, MALFORMED_MODEL_GROUPS_BODY),
            ),
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    /// The group answer is an upper bound: a listing that fails on `/models`
    /// never asks for it.
    pub const MODELS_UNAUTHORIZED: Fixture = Fixture {
        name: "models_unauthorized",
        script: &[
            Canned::at(MODELS_PATH, Canned::json(401, UNAUTHORIZED_BODY)),
            Canned::at(MODEL_GROUP_INFO_PATH, Canned::json(200, MODEL_GROUPS_BODY)),
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };

    /// `0.125` and `0.375` are exact binary ties for `{:.2}`, rounding opposite
    /// ways under half-to-even. The reset carries an offset and a fraction.
    const KEY_INFO_BODY: &str = r#"{"key":"88dc28d0f030c55ed4ab77ed8faf0981","info":{"key_name":"sk-...abcd","key_alias":"me@example.com","spend":0.125,"max_budget":0.375,"budget_duration":"30d","budget_reset_at":"2026-10-01T02:00:00.25+02:00","models":[],"metadata":{}}}"#;
    /// A null budget, a reset that is absent rather than null, and a negative
    /// spend.
    const NO_BUDGET_KEY_INFO_BODY: &str =
        r#"{"key":"k","info":{"key_alias":null,"spend":-0.5,"max_budget":null}}"#;
    /// Past the budget, so the percentage caps, with a reset that is no
    /// timestamp.
    const OVER_BUDGET_KEY_INFO_BODY: &str =
        r#"{"key":"k","info":{"spend":12.5,"max_budget":10,"budget_reset_at":"next month"}}"#;

    const ACTIVITY_BODY: &str = r#"{"daily_data":[{"date":"2026-09-23","metrics":{"api_requests":14,"total_tokens":5000000000}}],"sum_api_requests":14,"sum_total_tokens":5000000000}"#;
    const EMPTY_ACTIVITY_BODY: &str =
        r#"{"daily_data":[],"sum_api_requests":0,"sum_total_tokens":0}"#;
    const MALFORMED_ACTIVITY_BODY: &str = r#"{"sum_api_requests":14.0,"sum_total_tokens":29027}"#;

    /// Hourly rows out of name order. `glm5.2` sums two hours to the same
    /// micro-dollars `gpt-oss-120b` spends in one, a tie the name order breaks.
    /// `qwen3.5-9b` spends half a micro-dollar past twelve and counts tokens
    /// past `u32`. `llama-refund` is a negative spend.
    const SPEND_LOGS_BODY: &str = r#"{"data":[
        {"request_id":"h1","api_key":"k","model_group":"gpt-oss-120b","key_alias":"a","startTime":"2026-09-23T09:00:00+00:00","endTime":"2026-09-23T10:00:00+00:00","completionStartTime":null,"api_requests":1,"prompt_tokens":1000,"completion_tokens":200,"total_tokens":1200,"spend":0.0009,"request_duration_ms":3600000},
        {"request_id":"h2","api_key":"k","model_group":"qwen3-coder-next","key_alias":"a","startTime":"2026-09-23T09:00:00+00:00","endTime":"2026-09-23T10:00:00+00:00","completionStartTime":null,"api_requests":4,"prompt_tokens":15000,"completion_tokens":5000,"total_tokens":20000,"spend":0.01,"request_duration_ms":3600000},
        {"request_id":"h3","api_key":"k","model_group":"glm5.2","key_alias":"a","startTime":"2026-09-23T09:00:00+00:00","endTime":"2026-09-23T10:00:00+00:00","completionStartTime":null,"api_requests":1,"prompt_tokens":120,"completion_tokens":80,"total_tokens":200,"spend":0.0004,"request_duration_ms":3600000},
        {"request_id":"h4","api_key":"k","model_group":"qwen3-coder-next","key_alias":"a","startTime":"2026-09-23T10:00:00+00:00","endTime":"2026-09-23T11:00:00+00:00","completionStartTime":null,"api_requests":3,"prompt_tokens":5000,"completion_tokens":3000,"total_tokens":8000,"spend":0.0042,"request_duration_ms":3600000},
        {"request_id":"h5","api_key":"k","model_group":"glm5.2","key_alias":"a","startTime":"2026-09-23T10:00:00+00:00","endTime":"2026-09-23T11:00:00+00:00","completionStartTime":null,"api_requests":1,"prompt_tokens":80,"completion_tokens":80,"total_tokens":160,"spend":0.0005,"request_duration_ms":3600000},
        {"request_id":"h6","api_key":"k","model_group":"qwen3.5-9b","key_alias":"a","startTime":"2026-09-23T11:00:00+00:00","endTime":"2026-09-23T12:00:00+00:00","completionStartTime":null,"api_requests":1,"prompt_tokens":4294967296,"completion_tokens":1,"total_tokens":4294967297,"spend":0.0000125,"request_duration_ms":3600000},
        {"request_id":"h7","api_key":"k","model_group":"llama-refund","key_alias":"a","startTime":"2026-09-23T11:00:00+00:00","endTime":"2026-09-23T12:00:00+00:00","completionStartTime":null,"api_requests":0,"prompt_tokens":0,"completion_tokens":0,"total_tokens":0,"spend":-0.0005,"request_duration_ms":3600000}
    ]}"#;
    const EMPTY_SPEND_LOGS_BODY: &str = r#"{"data":[]}"#;
    const MALFORMED_SPEND_LOGS_BODY: &str = r#"{"data":[{"model_group":"glm5.2","prompt_tokens":-5,"completion_tokens":1,"total_tokens":1,"spend":0.001}]}"#;

    const ACTIVITY_OK: Canned = Canned::at(ACTIVITY_PATH, Canned::json(200, ACTIVITY_BODY));
    const SPEND_LOGS_OK: Canned = Canned::at(SPEND_LOGS_PATH, Canned::json(200, SPEND_LOGS_BODY));
    const EMPTY_ACTIVITY: Canned =
        Canned::at(ACTIVITY_PATH, Canned::json(200, EMPTY_ACTIVITY_BODY));
    const EMPTY_SPEND_LOGS: Canned =
        Canned::at(SPEND_LOGS_PATH, Canned::json(200, EMPTY_SPEND_LOGS_BODY));

    pub const USAGE: Fixture = Fixture {
        name: "usage",
        script: &[
            Canned::at(KEY_INFO_PATH, Canned::json(200, KEY_INFO_BODY)),
            ACTIVITY_OK,
            SPEND_LOGS_OK,
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    pub const USAGE_NO_BUDGET: Fixture = Fixture {
        name: "usage_no_budget",
        script: &[
            Canned::at(KEY_INFO_PATH, Canned::json(200, NO_BUDGET_KEY_INFO_BODY)),
            EMPTY_ACTIVITY,
            EMPTY_SPEND_LOGS,
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    pub const USAGE_OVER_BUDGET: Fixture = Fixture {
        name: "usage_over_budget",
        script: &[
            Canned::at(KEY_INFO_PATH, Canned::json(200, OVER_BUDGET_KEY_INFO_BODY)),
            EMPTY_ACTIVITY,
            EMPTY_SPEND_LOGS,
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    /// The key report alone still makes a usage answer.
    pub const USAGE_SIDE_CALLS_FAIL: Fixture = Fixture {
        name: "usage_side_calls_fail",
        script: &[
            Canned::at(KEY_INFO_PATH, Canned::json(200, KEY_INFO_BODY)),
            Canned::at(ACTIVITY_PATH, Canned::json(500, SERVER_ERROR_BODY)),
            Canned::at(SPEND_LOGS_PATH, Canned::json(500, SERVER_ERROR_BODY)),
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    /// A float where the activity counts are integers, and a negative token
    /// count in the spend logs: each answer is dropped whole, like a failure.
    pub const USAGE_SIDE_CALLS_MALFORMED: Fixture = Fixture {
        name: "usage_side_calls_malformed",
        script: &[
            Canned::at(KEY_INFO_PATH, Canned::json(200, KEY_INFO_BODY)),
            Canned::at(ACTIVITY_PATH, Canned::json(200, MALFORMED_ACTIVITY_BODY)),
            Canned::at(
                SPEND_LOGS_PATH,
                Canned::json(200, MALFORMED_SPEND_LOGS_BODY),
            ),
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    /// The side answers are an upper bound: a rejected key asks for neither.
    pub const USAGE_UNAUTHORIZED: Fixture = Fixture {
        name: "usage_unauthorized",
        script: &[
            Canned::at(KEY_INFO_PATH, Canned::json(401, UNAUTHORIZED_BODY)),
            ACTIVITY_OK,
            SPEND_LOGS_OK,
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };

    pub fn model() -> Model {
        Model::from_spec(MODEL_SPEC).expect(UNKNOWN_MODEL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::ProviderRegistry;

    #[test]
    fn manifest_lists_the_catalogued_default_model() {
        let spec = ProviderRegistry::get(SLUG).expect("regolo is a builtin");
        assert!(
            spec.models()
                .iter()
                .any(|m| m.prefixes == ["qwen3-coder-next"])
        );
    }
}
