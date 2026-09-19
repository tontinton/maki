use maki_config::providers::Protocol;

use crate::model::ModelFamily;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, NO_CURATED_MODELS,
    ProviderSpec,
};

const SLUG: &str = "openrouter";
const DISPLAY_NAME: &str = "OpenRouter";
const ENV_VAR: &str = "OPENROUTER_API_KEY";
const BASE_URL: &str = "https://openrouter.ai/api/v1";
const DEFAULT_MODEL: &str = "openrouter/openai/gpt-5.5";
const LOGIN_URL: &str = "https://openrouter.ai/keys";
const FEATURES: &str = "300+ models from all providers, prompt caching, provider routing";

const DISCOVERY_NOTE: &str = "OpenRouter aggregates models from many providers behind a single API key. \
     Browse available models at [openrouter.ai/models](https://openrouter.ai/models). \
     Use any model ID directly (e.g. `openrouter/anthropic/claude-sonnet-4`).";

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

/// The recorded cases the bundled `openrouter` plugin replays.
///
/// [`crate::model::ModelInfo::effort`] never reaches a golden, so the `models` listing alone
/// shows only what the `reasoning` block did to `supports_thinking`. What it
/// did to the effort dialect is pinned by the discovered turns, each of which
/// lists the one catalog below and then asks one of its models for a thinking
/// mode.
#[cfg(any(test, feature = "test-support"))]
pub mod fixtures {
    use crate::model::Model;
    use crate::providers::replay::{self, Fixture};
    use crate::test_support::Canned;
    use crate::{Effort, ThinkingConfig};

    /// Lists `xhigh` down to `minimal` plus two names maki has no level for.
    pub const XHIGH_SPEC: &str = "openrouter/openai/gpt-5.5";
    /// Lists nothing above `medium`, so a high ask snaps below the static
    /// dialect's ceiling.
    pub const MEDIUM_SPEC: &str = "openrouter/google/gemini-3-flash";
    /// Reasons by default and may be switched off.
    pub const DEFAULT_ENABLED_SPEC: &str = "openrouter/deepseek/deepseek-v4";
    /// Reasons by default and may not be switched off.
    pub const MANDATORY_SPEC: &str = "openrouter/anthropic/claude-opus-5";
    /// Has a `reasoning` block whose every effort name is one maki drops.
    pub const UNKNOWN_EFFORTS_SPEC: &str = "openrouter/moonshotai/kimi-k3";
    /// Has no `reasoning` block, only `reasoning` in `supported_parameters`.
    pub const PARAMETER_ONLY_SPEC: &str = "openrouter/qwen/qwen3-coder";
    /// Listed with no reasoning signal at all.
    pub const NO_REASONING_SPEC: &str = "openrouter/mistralai/codestral";
    /// Absent from [`CATALOG`].
    pub const UNLISTED_SPEC: &str = "openrouter/meta-llama/llama-5";

    /// 62% of the 64k thinking budget the fallback 128k output window allows,
    /// which converts to `xhigh`: past the static dialect's `high` ceiling, so
    /// only a discovered effort list lets it through.
    const BUDGET_TOKENS: u32 = 40_000;
    const SESSION: &str = "01965087-4c71-7f00-8000-000000000001";

    const UNKNOWN_MODEL: &str = "the model spec did not resolve";

    /// One `/models` answer for every discovered turn and for the listing
    /// itself. Besides the reasoning variants it carries each number the
    /// parser has to refuse or scale. `context_length` comes as a float, a
    /// negative, a null and past u32. Prices come as a bare number, an
    /// unparsable string, a negative and a sub-cent `0.125`. Some rows are for the modality filter
    /// to drop, and two share an id, which a stable sort keeps in arrival
    /// order.
    const CATALOG: &str = r#"{"data":[
{"id":"z-ai/glm-5","context_length":128000,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},"pricing":{"prompt":"0.0000006","completion":"0.0000022"},"reasoning":{"supported_efforts":"high"}},
{"id":"openai/gpt-5.5","context_length":400000,"architecture":{"input_modalities":["text","image","file"],"output_modalities":["text"]},"pricing":{"prompt":"0.00000125","completion":"0.00001","input_cache_read":"0.000000125"},"supported_parameters":["reasoning","tools"],"reasoning":{"supported_efforts":["xhigh","high","medium","low","minimal","none","bogus"],"default_enabled":false,"mandatory":false}},
{"id":"black-forest-labs/flux-2","architecture":{"input_modalities":["text"],"output_modalities":["image"]},"pricing":{"prompt":"0","completion":"0"}},
{"id":"deepseek/deepseek-v4","context_length":-1,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},"pricing":{"prompt":"n/a","completion":"0.0000011"},"reasoning":{"default_enabled":true,"mandatory":false}},
{"id":"anthropic/claude-opus-5","context_length":1000000.0,"architecture":{"input_modalities":["image","text"],"output_modalities":["text"]},"pricing":{"prompt":"0.000005","completion":"0.000025","input_cache_read":"0.0000005","input_cache_write":"0.00000625"},"reasoning":{"supported_efforts":[],"default_enabled":true,"mandatory":true}},
{"id":"openai/whisper-2","architecture":{"input_modalities":["audio"],"output_modalities":["text"]}},
{"id":"moonshotai/kimi-k3","context_length":null,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},"pricing":{"prompt":0.000003,"completion":"0.000015"},"reasoning":{"supported_efforts":["none","bogus"],"default_enabled":"true","mandatory":null}},
{"id":"qwen/qwen3-coder","context_length":5000000000,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},"pricing":{"prompt":"-1","completion":"-1","input_cache_read":"garbage"},"supported_parameters":["tools","reasoning"]},
{"id":"google/gemini-3-flash","context_length":1048576,"architecture":{"input_modalities":["text","image"],"output_modalities":["text","image"]},"pricing":{"prompt":"0.0000005","completion":"0.000003"},"reasoning":{"supported_efforts":[1,null,"LOW","medium","minimal","low"]}},
{"id":"mistralai/codestral","context_length":256000,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},"pricing":{"prompt":"0.0000003","completion":"0.0000009"},"supported_parameters":["tools"],"reasoning":null},
{"id":"x-ai/grok-5","context_length":2000000,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},"reasoning":true},
{"architecture":{"input_modalities":["text"],"output_modalities":["text"]}},
{"id":"openrouter/auto","architecture":{"input_modalities":["text"]}},
{"id":"meta-llama/llama-guard-5","context_length":131072},
{"id":"z-ai/glm-5","context_length":200000,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},"pricing":{"prompt":"0.0000006","completion":"0.0000022"}}
]}"#;

    const SUCCESS_TRANSCRIPT: &str = r#": OPENROUTER PROCESSING

data: {"id":"gen-1","choices":[{"delta":{"reasoning":"weighing the options"}}]}

data: {"id":"gen-1","choices":[{"delta":{"content":"Hello"}}]}

data: {"id":"gen-1","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}

data: {"id":"gen-1","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}

data: {"id":"gen-1","choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":4},"cost":0.00042}}

data: [DONE]

"#;

    const DISCOVERY_SCRIPT: &[Canned] =
        &[Canned::json(200, CATALOG), Canned::sse(SUCCESS_TRANSCRIPT)];

    const fn discovered(name: &'static str, thinking: ThinkingConfig) -> Fixture {
        Fixture {
            name,
            script: DISCOVERY_SCRIPT,
            thinking,
            session: None,
        }
    }

    pub const MODELS: Fixture = Fixture {
        name: "models",
        script: &[Canned::json(200, CATALOG)],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    pub const MODELS_UNAUTHORIZED: Fixture = Fixture {
        name: "models_unauthorized",
        script: replay::UNAUTHORIZED.script,
        thinking: ThinkingConfig::Off,
        session: None,
    };

    /// Replayed on [`XHIGH_SPEC`].
    pub const MAX_SNAPS_TO_XHIGH: Fixture =
        discovered("max_snaps_to_xhigh", ThinkingConfig::Effort(Effort::Max));
    /// Replayed on [`MEDIUM_SPEC`].
    pub const MAX_SNAPS_TO_MEDIUM: Fixture =
        discovered("max_snaps_to_medium", ThinkingConfig::Effort(Effort::Max));
    /// Replayed on [`UNKNOWN_EFFORTS_SPEC`]: an emptied list inherits the
    /// declared levels.
    pub const MAX_WITH_UNKNOWN_EFFORTS_ONLY: Fixture = discovered(
        "max_with_unknown_efforts_only",
        ThinkingConfig::Effort(Effort::Max),
    );
    /// Replayed on [`PARAMETER_ONLY_SPEC`].
    pub const MAX_WITH_PARAMETER_ONLY_REASONING: Fixture = discovered(
        "max_with_parameter_only_reasoning",
        ThinkingConfig::Effort(Effort::Max),
    );
    /// Replayed on [`UNLISTED_SPEC`]: plain `prefer-high`.
    pub const UNDISCOVERED: Fixture =
        discovered("undiscovered", ThinkingConfig::Effort(Effort::Max));
    /// Replayed on [`DEFAULT_ENABLED_SPEC`], which is told `none`.
    pub const OFF_DEFAULT_ENABLED: Fixture = discovered("off_default_enabled", ThinkingConfig::Off);
    /// Replayed on [`MANDATORY_SPEC`], which is told nothing.
    pub const OFF_MANDATORY: Fixture = discovered("off_mandatory", ThinkingConfig::Off);
    /// Replayed on [`XHIGH_SPEC`].
    pub const BUDGET: Fixture = discovered("budget", ThinkingConfig::Budget(BUDGET_TOKENS));
    /// Replayed on [`NO_REASONING_SPEC`]: no support, so no effort at all.
    pub const NOT_A_REASONING_MODEL: Fixture = discovered(
        "not_a_reasoning_model",
        ThinkingConfig::Effort(Effort::High),
    );
    /// Replayed on [`XHIGH_SPEC`]. The session rides in the body.
    pub const IN_SESSION: Fixture = Fixture {
        session: Some(SESSION),
        ..discovered("in_session", ThinkingConfig::Effort(Effort::High))
    };

    pub fn model(spec: &str) -> Model {
        Model::from_spec(spec).expect(UNKNOWN_MODEL)
    }
}
