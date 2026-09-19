use maki_config::providers::Protocol;

use crate::model::ModelFamily;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, ProviderSpec,
};

const SLUG: &str = "synthetic";
const DISPLAY_NAME: &str = "Synthetic";
const ENV_VAR: &str = "SYNTHETIC_API_KEY";
const BASE_URL: &str = "https://api.synthetic.new/openai/v1";
const DEFAULT_MODEL: &str = "synthetic/hf:moonshotai/Kimi-K2.5";
const LOGIN_URL: &str = "https://synthetic.new";
const FEATURES: &str = "Reasoning effort support (low/medium/high), open-weight models";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Synthetic,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(32_000),
    fallback_context_window: 128_000,
    models_toml: include_str!("../../models/synthetic.toml"),
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

/// The recorded cases the bundled `synthetic` plugin replays.
///
/// Every case was recorded while the bespoke `impl Provider` this module used
/// to hold was still here. The impl is gone and the artifacts are not, so each
/// fixture still pins the bytes and the events that provider produced on the
/// day it was ported.
#[cfg(any(test, feature = "test-support"))]
pub mod fixtures {
    use crate::model::Model;
    use crate::providers::replay::Fixture;
    use crate::test_support::Canned;
    use crate::{Effort, ThinkingConfig};

    pub const MODEL_SPEC: &str = "synthetic/hf:moonshotai/Kimi-K2.5";
    /// Reaches the wire as `reasoning_effort`, which is the one thing
    /// the declared `thinking` dialect is there to do.
    const EFFORT: Effort = Effort::High;
    const UNKNOWN_MODEL: &str = "the curated table has no such model";

    pub fn model() -> Model {
        Model::from_spec(MODEL_SPEC).expect(UNKNOWN_MODEL)
    }

    const SUCCESS_TRANSCRIPT: &str = r#"data: {"choices":[{"delta":{"reasoning_content":"weighing the options"}}]}

data: {"choices":[{"delta":{"content":"Hello"}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":4}}}

data: [DONE]

"#;

    pub const SUCCESS: Fixture = Fixture {
        name: "success",
        script: &[Canned::sse(SUCCESS_TRANSCRIPT)],
        thinking: ThinkingConfig::Effort(EFFORT),
        session: None,
    };
}
