use maki_config::providers::Protocol;

use crate::model::ModelFamily;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, NO_CURATED_MODELS,
    ProviderSpec,
};

const SLUG: &str = "requesty";
const DISPLAY_NAME: &str = "Requesty";
const ENV_VAR: &str = "REQUESTY_API_KEY";
const BASE_URL: &str = "https://router.requesty.ai/v1";
const DEFAULT_MODEL: &str = "requesty/openai/gpt-5.5";
const LOGIN_URL: &str = "https://app.requesty.ai/api-keys";
const FEATURES: &str = "700+ models behind one key, curated managed routing policies, EU region via `REQUESTY_BASE_URL`";

const DISCOVERY_NOTE: &str = "Requesty routes 700+ models from many providers behind a single API key. \
     Models are listed live from the API: curated managed policies first \
     (short ids such as `requesty/claude-sonnet-4-5` or `requesty/gpt-5.4-mini`, \
     `@eu` variants route only through EU providers), then the full \
     `<vendor>/<model>` catalog (e.g. `requesty/openai/gpt-4o-mini`). \
     Get a key at [app.requesty.ai/api-keys](https://app.requesty.ai/api-keys). \
     Set `REQUESTY_BASE_URL=https://router.eu.requesty.ai/v1` to keep all \
     traffic in the EU.";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(128_000),
    fallback_context_window: 200_000,
    models_toml: NO_CURATED_MODELS,
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
        catalog: CatalogDoc::Discovered(DISCOVERY_NOTE),
        trailing_notes: &[],
    },
};

inventory::submit!(SPEC.config_row());

/// The recorded cases the bundled `requesty` plugin replays.
///
/// The two catalog fetches run concurrently, so every script that answers
/// them is routed by path: arrival order is a race, and a sequential script
/// would hand each listing the other's answer.
#[cfg(any(test, feature = "test-support"))]
pub mod fixtures {
    use crate::model::{Model, ThinkingSupport};
    use crate::providers::replay::Fixture;
    use crate::test_support::Canned;
    use crate::{Effort, ThinkingConfig};

    /// Thinking-capable, so the effort reaches the wire.
    const THINKING_SPEC: &str = "requesty/openai/gpt-5.5";
    /// Listed with `supports_reasoning: false` in the catalog listing, which is
    /// what [`DISCOVERED_NON_THINKING`] leans on.
    pub const NON_THINKING_SPEC: &str = "requesty/openai/gpt-4o-mini";
    /// `prefer-high` tops out at `high`, so the golden shows the snap.
    const EFFORT: Effort = Effort::Max;

    const UNKNOWN_MODEL: &str = "the model spec did not resolve";

    const MANAGED_PATH: &str = "/v1/models/managed";
    const CATALOG_PATH: &str = "/v1/models";
    const CHAT_PATH: &str = "/v1/chat/completions";

    const UNAUTHORIZED_BODY: &str = r#"{"error":{"message":"invalid api key"}}"#;
    const SERVER_ERROR_BODY: &str = r#"{"error":{"message":"internal error"}}"#;

    const SUCCESS_TRANSCRIPT: &str = r#"data: {"choices":[{"delta":{"reasoning_content":"weighing the options"}}]}

data: {"choices":[{"delta":{"content":"Hello"}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":4}}}

data: [DONE]

"#;

    /// Unsorted on purpose, with an id listed twice (equal sort keys, both
    /// kept in arrival order), a zero limit, a price as a string, a flag that
    /// is not a bool, a null, and two entries with no usable id.
    const MANAGED_BODY: &str = r#"{"data":[
{"id":"gpt-5.4-mini","api":"chat","context_window":400000,"max_output_tokens":128000,"input_price":0.00000025,"output_price":0.000002,"cached_price":0.000000025,"supports_reasoning":true,"supports_vision":true},
{"id":"claude-sonnet-4-5","api":"chat","context_window":200000,"max_output_tokens":64000,"input_price":0.000003,"output_price":0.000015,"caching_price":0.00000375,"cached_price":0.0000003,"supports_reasoning":true,"supports_vision":true},
{"id":"claude-sonnet-4-5","api":"chat","context_window":1000000,"max_output_tokens":64000,"input_price":0.000006,"output_price":0.0000225,"supports_reasoning":true,"supports_vision":true},
{"id":"claude-sonnet-4-5@eu","api":"chat","context_window":0,"max_output_tokens":0,"input_price":"0.000003","output_price":0.000015,"supports_reasoning":null,"supports_vision":"yes"},
{"api":"chat","context_window":128000},
{"id":42,"api":"chat"}
]}"#;

    /// Shares `gpt-5.4-mini` with `MANAGED_BODY` (the managed row wins),
    /// lists `vendor/twin` twice (merge keeps the first), and carries a non
    /// chat api, whole and fractional floats for limits, negatives, a limit
    /// past `u32`, a `.125` price, all-null fields, unreadable prices, a zero
    /// price, an entry with no `api` and one that is not an object.
    const CATALOG_BODY: &str = r#"{"data":[
{"id":"vendor/twin","api":"chat","context_window":32000,"input_price":0,"output_price":0},
{"id":"openai/text-embedding-3-small","api":"embedding","context_window":8191,"input_price":0.00000002,"output_price":0},
{"id":"openai/gpt-4o-mini","api":"chat","context_window":128000,"max_output_tokens":16384,"input_price":0.00000015,"output_price":0.0000006,"cached_price":0.000000075,"supports_reasoning":false,"supports_vision":true},
{"id":"gpt-5.4-mini","api":"chat","context_window":272000,"max_output_tokens":32000,"input_price":0.0000005,"output_price":0.000004,"supports_reasoning":false,"supports_vision":false},
{"id":"vendor/floats","api":"chat","context_window":131072.0,"max_output_tokens":8192.5,"input_price":0.000000125,"output_price":0.000000375,"supports_reasoning":1},
{"id":"vendor/negatives","api":"chat","context_window":-1,"max_output_tokens":4294967296,"input_price":-0.000001,"output_price":0.000002},
{"id":"vendor/nulls","api":null,"context_window":null,"max_output_tokens":null,"input_price":null,"output_price":null,"caching_price":null,"cached_price":null,"supports_reasoning":null,"supports_vision":null},
{"id":"vendor/bad-price","api":"chat","context_window":65536,"input_price":"free","output_price":{"per_token":0.000001},"cached_price":"0.1"},
{"id":"vendor/half-cache","input_price":0.000001,"output_price":0.000002,"caching_price":"n/a"},
{"id":"vendor/twin","api":"chat","context_window":64000,"input_price":0.000001,"output_price":0.000002},
"stray"
]}"#;

    const SUCCESS_SCRIPT: &[Canned] = &[Canned::sse(SUCCESS_TRANSCRIPT)];

    pub const SUCCESS: Fixture = Fixture {
        name: "success",
        script: SUCCESS_SCRIPT,
        thinking: ThinkingConfig::Effort(EFFORT),
        session: None,
    };
    /// The same effort on a model without thinking: `reasoning_effort` stays
    /// off the wire.
    pub const SUCCESS_NON_THINKING: Fixture = Fixture {
        name: "success_non_thinking",
        script: SUCCESS_SCRIPT,
        thinking: ThinkingConfig::Effort(EFFORT),
        session: None,
    };
    /// The gate fed by discovery rather than an override: the listing marks
    /// [`NON_THINKING_SPEC`] as not reasoning, so the turn after it sends no
    /// effort.
    pub const DISCOVERED_NON_THINKING: Fixture = Fixture {
        name: "discovered_non_thinking",
        script: &[
            Canned::at(MANAGED_PATH, Canned::json(200, MANAGED_BODY)),
            Canned::at(CATALOG_PATH, Canned::json(200, CATALOG_BODY)),
            Canned::at(CHAT_PATH, Canned::sse(SUCCESS_TRANSCRIPT)),
        ],
        thinking: ThinkingConfig::Effort(EFFORT),
        session: None,
    };
    pub const MODELS: Fixture = Fixture {
        name: "models",
        script: &[
            Canned::at(MANAGED_PATH, Canned::json(200, MANAGED_BODY)),
            Canned::at(CATALOG_PATH, Canned::json(200, CATALOG_BODY)),
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    /// Lists the catalog alone.
    pub const MODELS_MANAGED_DOWN: Fixture = Fixture {
        name: "models_managed_down",
        script: &[
            Canned::at(MANAGED_PATH, Canned::json(500, SERVER_ERROR_BODY)),
            Canned::at(CATALOG_PATH, Canned::json(200, CATALOG_BODY)),
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    /// Lists the managed policies alone.
    pub const MODELS_CATALOG_DOWN: Fixture = Fixture {
        name: "models_catalog_down",
        script: &[
            Canned::at(MANAGED_PATH, Canned::json(200, MANAGED_BODY)),
            Canned::at(CATALOG_PATH, Canned::json(500, SERVER_ERROR_BODY)),
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    /// Two different failures, so the golden shows it is the managed one that
    /// surfaces.
    pub const MODELS_BOTH_DOWN: Fixture = Fixture {
        name: "models_both_down",
        script: &[
            Canned::at(MANAGED_PATH, Canned::json(401, UNAUTHORIZED_BODY)),
            Canned::at(CATALOG_PATH, Canned::json(500, SERVER_ERROR_BODY)),
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };

    pub fn model(spec: &str) -> Model {
        Model::from_spec(spec).expect(UNKNOWN_MODEL)
    }

    /// Support is pinned rather than left to discovery or the models.dev
    /// cache, so the gate reads the same in every process.
    fn pinned(spec: &str, support: ThinkingSupport) -> Model {
        let mut model = model(spec);
        model.thinking_override = Some(support);
        model
    }

    pub fn thinking_model() -> Model {
        pinned(THINKING_SPEC, ThinkingSupport::Yes)
    }

    pub fn non_thinking_model() -> Model {
        pinned(NON_THINKING_SPEC, ThinkingSupport::No)
    }
}
