use maki_config::providers::{Protocol, ProviderPlan};

use crate::model::ModelFamily;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, ProviderSpec,
};

const SLUG: &str = "mistral";
const DISPLAY_NAME: &str = "Mistral";
const ENV_VAR: &str = "MISTRAL_API_KEY";
const BASE_URL: &str = "https://api.mistral.ai/v1";
const DEFAULT_MODEL: &str = "mistral/mistral-medium-latest";
const CODING_MODEL: &str = "mistral/mistral-vibe-cli-latest";
const LOGIN_URL: &str = "https://admin.mistral.ai/organization/api-keys";

const PLANS: &[(&str, ProviderPlan)] = &[
    (
        "standard",
        ProviderPlan {
            display_name: "Standard",
            base_url: BASE_URL,
            default_model: Some(DEFAULT_MODEL),
            login_url: None,
        },
    ),
    (
        "coding",
        ProviderPlan {
            display_name: "Vibe / Coding",
            base_url: BASE_URL,
            default_model: Some(CODING_MODEL),
            login_url: Some("https://console.mistral.ai/codestral/cli"),
        },
    ),
];

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
    fallback_context_window: 128_000,
    models_toml: include_str!("../../models/mistral.toml"),
    pricing_schedule: None,
    build: Build::Declared,
    aperture: Some(ApertureRoute {
        path_prefix: DEFAULT_PATH_PREFIX,
    }),
    login: Some(LoginConfig {
        protocol: Protocol::Openai,
        default_base_url: BASE_URL,
        default_model: DEFAULT_MODEL,
        plans: Some(PLANS),
        login_url: Some(LOGIN_URL),
        needs_url: false,
    }),
    docs: GeneratedDocs {
        api_urls: &[BASE_URL],
        features: None,
        auth: AuthDoc::EnvVar,
        catalog: CatalogDoc::Table,
        trailing_notes: &[],
    },
};

inventory::submit!(SPEC.config_row());

/// The recorded cases the bundled `mistral` plugin replays.
#[cfg(any(test, feature = "test-support"))]
pub mod fixtures {
    use serde_json::json;

    use crate::model::Model;
    use crate::providers::replay::Fixture;
    use crate::test_support::Canned;
    use crate::types::ContentBlock;
    use crate::{Effort, Message, Role, ThinkingConfig};

    const MODEL_SPEC: &str = "mistral/mistral-medium-latest";
    /// The one level [`crate::dialect::HIGH_ONLY`] sends.
    const EFFORT: Effort = Effort::High;
    /// Travels as `x-affinity`, verbatim.
    const SESSION: &str = "0192f0c4-6b1e-7c3a-9d2e-5f8a1b3c4d5e";

    const UNKNOWN_MODEL: &str = "the curated table has no such model";

    const FIRST_ASK: &str = "read a.txt";
    const KEPT_REASONING: &str = "a.txt first";
    const REPLY: &str = "on it";
    const LONE_REASONING: &str = "nothing to say yet";
    const TOOL_NAME: &str = "read";
    const BARE_TOOL_ID: &str = "call_1";
    const BARE_TOOL_PATH: &str = "a.txt";
    const BARE_TOOL_OUTPUT: &str = "contents of a.txt";
    const REASONED_TOOL_ID: &str = "call_2";
    const REASONED_TOOL_PATH: &str = "b.txt";
    const REASONED_TOOL_REASONING: &str = "b.txt next";
    const REASONED_TOOL_OUTPUT: &str = "contents of b.txt";
    const PLAIN_REPLY: &str = "both read";
    const FOLLOW_UP: &str = "now summarise them";

    /// Mistral's own shape for reasoning: `content` as an array of `thinking`
    /// and `text` parts, a thinking part given as a bare string as well as a
    /// block, then a plain string delta and a tool call sent whole.
    const SUCCESS_TRANSCRIPT: &str = r#"data: {"choices":[{"delta":{"role":"assistant","content":[{"type":"thinking","thinking":[{"type":"text","text":"weighing the options"}]}]}}]}

data: {"choices":[{"delta":{"content":[{"type":"thinking","thinking":[" and a plan"]},{"type":"text","text":"Hello"}]}}]}

data: {"choices":[{"delta":{"content":" there"}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"read","arguments":"{\"path\":\"c.txt\"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17,"prompt_tokens_details":{"cached_tokens":4}}}

data: [DONE]

"#;

    /// Only `completion_chat` rows survive, sorted by id, and every field the
    /// parser reads is given each shape it must reject: a float, a negative,
    /// a null, a string and one past `u32::MAX` for `max_context_length`
    /// (with `u32::MAX` itself kept), non-bool capability flags, and a
    /// missing or non-string id. The two `mistral-large-latest` rows pin the
    /// stable sort.
    const MODELS_BODY: &str = r#"{"object":"list","data":[
{"id":"mistral-medium-latest","object":"model","capabilities":{"completion_chat":true,"function_calling":true,"reasoning":true,"vision":true},"max_context_length":262144},
{"id":"mistral-large-latest","object":"model","capabilities":{"completion_chat":true,"vision":true},"max_context_length":131072},
{"id":"codestral-latest","object":"model","capabilities":{"completion_chat":true,"reasoning":false,"vision":false},"max_context_length":256000.0},
{"id":"mistral-embed","object":"model","capabilities":{"completion_chat":false},"max_context_length":8192},
{"id":"ministral-14b-latest","object":"model","capabilities":{"completion_chat":true},"max_context_length":-1},
{"id":"mistral-ocr-latest","object":"model","capabilities":{},"max_context_length":32768},
{"id":"magistral-medium-latest","object":"model","capabilities":{"completion_chat":true,"reasoning":true,"vision":"yes"},"max_context_length":4294967296},
{"id":"pixtral-large-latest","object":"model","capabilities":{"completion_chat":"true","vision":true},"max_context_length":131072},
{"id":"mistral-small-latest","object":"model","capabilities":{"completion_chat":true,"reasoning":null,"vision":null},"max_context_length":null},
{"id":"devstral-medium-latest","object":"model","capabilities":{"completion_chat":true,"vision":false},"max_context_length":4294967295},
{"id":"open-mistral-nemo","object":"model","capabilities":{"completion_chat":true,"reasoning":"false"},"max_context_length":"131072"},
{"id":"mistral-moderation-latest","object":"model","max_context_length":8192},
{"object":"model","capabilities":{"completion_chat":true},"max_context_length":32768},
{"id":42,"object":"model","capabilities":{"completion_chat":true},"max_context_length":32768},
{"id":"mistral-large-latest","object":"model","capabilities":{"completion_chat":true,"reasoning":true},"max_context_length":128000}
]}"#;

    const UNAUTHORIZED_BODY: &str = r#"{"message":"Unauthorized","request_id":"req_replay"}"#;

    const SUCCESS_SCRIPT: &[Canned] = &[Canned::sse(SUCCESS_TRANSCRIPT)];

    pub const SUCCESS: Fixture = Fixture {
        name: "success",
        script: SUCCESS_SCRIPT,
        thinking: ThinkingConfig::Effort(EFFORT),
        session: None,
    };
    /// [`crate::dialect::HIGH_ONLY`] has no off string, so nothing is sent.
    pub const THINKING_OFF: Fixture = Fixture {
        name: "thinking_off",
        script: SUCCESS_SCRIPT,
        thinking: ThinkingConfig::Off,
        session: None,
    };
    pub const IN_SESSION: Fixture = Fixture {
        name: "in_session",
        script: SUCCESS_SCRIPT,
        thinking: ThinkingConfig::Effort(EFFORT),
        session: Some(SESSION),
    };
    /// Replayed with [`history`], so the assistant-turn rewrite has turns to
    /// act on.
    pub const HISTORY: Fixture = Fixture {
        name: "history",
        script: SUCCESS_SCRIPT,
        thinking: ThinkingConfig::Effort(EFFORT),
        session: None,
    };
    pub const MODELS: Fixture = Fixture {
        name: "models",
        script: &[Canned::json(200, MODELS_BODY)],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    /// The second answer records a retry of the rejected key instead of
    /// parking on it.
    pub const MODELS_UNAUTHORIZED: Fixture = Fixture {
        name: "models_unauthorized",
        script: &[
            Canned::json(401, UNAUTHORIZED_BODY),
            Canned::json(401, UNAUTHORIZED_BODY),
        ],
        thinking: ThinkingConfig::Off,
        session: None,
    };

    pub fn model() -> Model {
        Model::from_spec(MODEL_SPEC).expect(UNKNOWN_MODEL)
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        }
    }

    fn tool_use(id: &str, path: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_owned(),
            name: TOOL_NAME.to_owned(),
            input: json!({ "path": path }),
            thought_signature: None,
        }
    }

    fn thinking(text: &str) -> ContentBlock {
        ContentBlock::Thinking {
            thinking: text.to_owned(),
            signature: None,
        }
    }

    fn tool_result(id: &str, output: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_owned(),
                content: output.to_owned(),
                is_error: false,
            }],
            ..Default::default()
        }
    }

    /// One of each assistant turn the rewrite has an opinion about: text with
    /// reasoning, reasoning with empty text, a bare tool call, a tool call
    /// with reasoning, and plain text that must come back untouched. Missing
    /// and array `content` never leave the openai codec, so only the unit
    /// tests reach them.
    pub fn history() -> Vec<Message> {
        vec![
            Message::user(FIRST_ASK.to_owned()),
            assistant(vec![
                thinking(KEPT_REASONING),
                ContentBlock::Text {
                    text: REPLY.to_owned(),
                },
            ]),
            assistant(vec![thinking(LONE_REASONING)]),
            assistant(vec![tool_use(BARE_TOOL_ID, BARE_TOOL_PATH)]),
            tool_result(BARE_TOOL_ID, BARE_TOOL_OUTPUT),
            assistant(vec![
                thinking(REASONED_TOOL_REASONING),
                tool_use(REASONED_TOOL_ID, REASONED_TOOL_PATH),
            ]),
            tool_result(REASONED_TOOL_ID, REASONED_TOOL_OUTPUT),
            assistant(vec![ContentBlock::Text {
                text: PLAIN_REPLY.to_owned(),
            }]),
            Message::user(FOLLOW_UP.to_owned()),
        ]
    }
}
