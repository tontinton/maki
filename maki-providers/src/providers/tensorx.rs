use maki_config::providers::Protocol;

use crate::model::ModelFamily;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GENERIC_DISCOVERY_NOTE, GeneratedDocs, LoginConfig,
    NO_CURATED_MODELS, ProviderSpec,
};

const SLUG: &str = "tensorx";
const DISPLAY_NAME: &str = "TensorX";
const ENV_VAR: &str = "TENSORX_API_KEY";
const BASE_URL: &str = "https://api.tensorx.ai/v1";
const DEFAULT_MODEL: &str = "tensorx/z-ai/glm-5.2";
const LOGIN_URL: &str = "https://tensorx.ai";
const FEATURES: &str = "Open-weight models, zero data retention, prompt caching";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
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
        catalog: CatalogDoc::Discovered(GENERIC_DISCOVERY_NOTE),
        trailing_notes: &[],
    },
};

inventory::submit!(SPEC.config_row());

/// The recorded cases the bundled `tensorx` plugin replays.
///
/// What a turn puts on the wire depends on what `/model/info` said about the
/// model, so every stream fixture past the shared failures is a discovery
/// run: the listing answer first, the stream answer second, from one script.
#[cfg(any(test, feature = "test-support"))]
pub mod fixtures {
    use crate::model::Model;
    use crate::providers::replay::Fixture;
    use crate::test_support::Canned;
    use crate::{Effort, ThinkingConfig};

    /// Advertises `thinking` alone, so the turn carries the bool.
    pub const THINKING_PARAM_SPEC: &str = "tensorx/z-ai/glm-5.2";
    /// Advertises `reasoning_effort` alone, so the turn carries the effort.
    pub const REASONING_EFFORT_SPEC: &str = "tensorx/openai/gpt-oss-120b";
    pub const BOTH_KNOBS_SPEC: &str = "tensorx/qwen/qwen3.5-397b";
    /// Advertises neither knob, so thinking goes through the chat template.
    pub const DEEPSEEK_V4_SPEC: &str = "tensorx/deepseek/deepseek-flash";
    /// The one DeepSeek id outside the V4 protocol, which gets no template
    /// toggle either.
    pub const DEEPSEEK_REASONER_SPEC: &str = "tensorx/deepseek/deepseek-reasoner";
    /// Absent from every listing below.
    pub const UNLISTED_SPEC: &str = "tensorx/moonshotai/kimi-k3";
    /// Absent from every listing too, and still a V4 id, which is all the
    /// template toggle keys off.
    pub const UNLISTED_DEEPSEEK_V4_SPEC: &str = "tensorx/deepseek/deepseek-v9-turbo";
    /// Above what [`crate::dialect::TENSORX`] accepts, so the effort on the
    /// wire proves it snaps rather than passes through.
    const EFFORT: Effort = Effort::Max;

    const UNKNOWN_MODEL: &str = "the model spec did not resolve";

    /// Every kind of model the stream path tells apart, then the non-chat
    /// entries the parser drops, then one entry per edge of each field it
    /// reads: a float, a negative, a null and an out-of-u32 count, string and
    /// wrong-typed values, a price that lands on a rounding tie, and two rows
    /// sharing an id so the sort has to be stable. Entries arrive unsorted.
    const MODELS_BODY: &str = r#"{"data":[
{"model_name":"openai/gpt-oss-120b","model_info":{"mode":"chat","max_input_tokens":131072,"max_output_tokens":32768,"input_cost_per_token":1.5e-7,"output_cost_per_token":6e-7,"supports_reasoning":true,"supported_openai_params":["max_tokens","tools","reasoning_effort"]}},
{"model_name":"z-ai/glm-5.2","model_info":{"mode":"chat","max_tokens":202752,"max_input_tokens":200000,"max_output_tokens":131072,"input_cost_per_token":6e-7,"output_cost_per_token":2.2e-6,"cache_read_input_token_cost":1.1e-7,"supports_vision":false,"supports_reasoning":true,"supported_openai_params":["max_tokens","tools","thinking"]}},
{"model_name":"edge/duplicate","model_info":{"mode":"chat","max_tokens":1000}},
{"model_name":"qwen/qwen3.5-397b","model_info":{"mode":"chat","max_tokens":262144,"supports_vision":true,"supports_reasoning":true,"supported_openai_params":["thinking","reasoning_effort"]}},
{"model_name":"deepseek/deepseek-flash","model_info":{"mode":"chat","max_tokens":1000000,"max_output_tokens":384000,"input_cost_per_token":2.8e-7,"output_cost_per_token":4.2e-7,"cache_creation_input_token_cost":0,"cache_read_input_token_cost":2.8e-8,"supports_reasoning":true,"supported_openai_params":["max_tokens","tools"]}},
{"model_name":"deepseek/deepseek-reasoner","model_info":{"mode":"chat","max_tokens":131072,"supports_reasoning":true,"supported_openai_params":["tools"]}},
{"model_name":"moonshotai/kimi-k3","model_info":{"max_tokens":262144,"supports_vision":true}},
{"model_name":"qwen/qwen3-embedding","model_info":{"mode":"embedding","max_tokens":8192}},
{"model_name":"black-forest-labs/flux","model_info":{"mode":"image_generation"}},
{"model_name":"edge/float-counts","model_info":{"mode":"chat","max_tokens":131072.0,"max_input_tokens":65536,"max_output_tokens":8192.5}},
{"model_name":"edge/negative","model_info":{"mode":"chat","max_tokens":-1,"max_input_tokens":-1,"max_output_tokens":-5,"input_cost_per_token":-1e-6}},
{"model_name":"edge/null","model_info":{"mode":null,"max_tokens":null,"max_input_tokens":32768,"max_output_tokens":null,"input_cost_per_token":null,"output_cost_per_token":1e-6,"supports_vision":null,"supports_reasoning":null,"supported_openai_params":null}},
{"model_name":"edge/beyond-u32","model_info":{"mode":"chat","max_tokens":5000000000,"max_input_tokens":131072,"max_output_tokens":4294967296}},
{"model_name":"edge/wrong-types","model_info":{"mode":1,"input_cost_per_token":"0.000001","output_cost_per_token":"abc","supports_vision":"true","supports_reasoning":"yes","supported_openai_params":"thinking"}},
{"model_name":"edge/mixed-params","model_info":{"mode":"chat","supported_openai_params":["thinking",1,null,{"name":"reasoning_effort"}]}},
{"model_name":"edge/rounding-tie","model_info":{"mode":"chat","input_cost_per_token":1.25e-7,"output_cost_per_token":3.75e-7,"cache_read_input_token_cost":1.5e-8}},
{"model_name":"edge/null-info","model_info":null},
{"model_name":"edge/duplicate","model_info":{"mode":"chat","max_tokens":2000}},
{"model_name":42,"model_info":{"mode":"chat"}},
{"model_info":{"mode":"chat"}},
{"model_name":"edge/no-info"},
null,
"stray"
]}"#;

    /// Only the models the discovery fixtures stream against, each carrying
    /// the one field the stream path reads.
    const DISCOVERY_BODY: &str = r#"{"data":[
{"model_name":"z-ai/glm-5.2","model_info":{"mode":"chat","supported_openai_params":["max_tokens","tools","thinking"]}},
{"model_name":"openai/gpt-oss-120b","model_info":{"mode":"chat","supported_openai_params":["max_tokens","tools","reasoning_effort"]}},
{"model_name":"qwen/qwen3.5-397b","model_info":{"mode":"chat","supported_openai_params":["thinking","reasoning_effort"]}},
{"model_name":"deepseek/deepseek-flash","model_info":{"mode":"chat","supported_openai_params":["max_tokens","tools"]}},
{"model_name":"deepseek/deepseek-reasoner","model_info":{"mode":"chat","supported_openai_params":["tools"]}}
]}"#;

    /// No `data` array at all, which lists nothing rather than failing.
    const NO_DATA_BODY: &str = r#"{"object":"list"}"#;

    const UNAUTHORIZED_BODY: &str = r#"{"error":{"message":"Authentication Error, Invalid proxy server token passed.","type":"auth_error","param":"None","code":"401"}}"#;

    const SUCCESS_TRANSCRIPT: &str = r#"data: {"choices":[{"delta":{"reasoning_content":"weighing the options"}}]}

data: {"choices":[{"delta":{"content":"Hello"}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":4}}}

data: [DONE]

"#;

    const DISCOVERED_SCRIPT: &[Canned] = &[
        Canned::json(200, DISCOVERY_BODY),
        Canned::sse(SUCCESS_TRANSCRIPT),
    ];

    pub const MODELS: Fixture = Fixture {
        name: "models",
        script: &[Canned::json(200, MODELS_BODY)],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    pub const MODELS_WITHOUT_DATA: Fixture = Fixture {
        name: "models_without_data",
        script: &[Canned::json(200, NO_DATA_BODY)],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    pub const MODELS_UNAUTHORIZED: Fixture = Fixture {
        name: "models_unauthorized",
        script: &[Canned::json(401, UNAUTHORIZED_BODY)],
        thinking: ThinkingConfig::Off,
        session: None,
    };

    const fn discovered(name: &'static str, thinking: ThinkingConfig) -> Fixture {
        Fixture {
            name,
            script: DISCOVERED_SCRIPT,
            thinking,
            session: None,
        }
    }

    pub const THINKING_PARAM: Fixture =
        discovered("thinking_param", ThinkingConfig::Effort(EFFORT));
    pub const THINKING_PARAM_OFF: Fixture = discovered("thinking_param_off", ThinkingConfig::Off);
    pub const REASONING_EFFORT: Fixture =
        discovered("reasoning_effort", ThinkingConfig::Effort(EFFORT));
    /// [`crate::dialect::TENSORX`] spells off out loud, as `none`.
    pub const REASONING_EFFORT_OFF: Fixture =
        discovered("reasoning_effort_off", ThinkingConfig::Off);
    pub const BOTH_KNOBS: Fixture = discovered("both_knobs", ThinkingConfig::Effort(EFFORT));
    pub const DEEPSEEK_V4: Fixture = discovered("deepseek_v4", ThinkingConfig::Effort(EFFORT));
    pub const DEEPSEEK_V4_OFF: Fixture = discovered("deepseek_v4_off", ThinkingConfig::Off);
    pub const DEEPSEEK_REASONER: Fixture =
        discovered("deepseek_reasoner", ThinkingConfig::Effort(EFFORT));
    pub const UNDISCOVERED: Fixture = discovered("undiscovered", ThinkingConfig::Effort(EFFORT));
    pub const UNDISCOVERED_DEEPSEEK_V4: Fixture =
        discovered("undiscovered_deepseek_v4", ThinkingConfig::Effort(EFFORT));

    pub fn model(spec: &str) -> Model {
        Model::from_spec(spec).expect(UNKNOWN_MODEL)
    }
}
