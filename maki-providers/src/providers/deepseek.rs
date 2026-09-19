use maki_config::providers::Protocol;

use crate::model::ModelFamily;
use crate::pricing::{PricingSchedule, PricingWindow};
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, ProviderSpec,
};

const SLUG: &str = "deepseek";
const DISPLAY_NAME: &str = "DeepSeek";
const ENV_VAR: &str = "DEEPSEEK_API_KEY";
const BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek/deepseek-flash";
const LOGIN_URL: &str = "https://platform.deepseek.com/api_keys";
const FEATURES: &str = "Thinking mode toggle (on/off), open-weight models";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(384_000),
    fallback_context_window: 1_000_000,
    models_toml: include_str!("../../models/deepseek.toml"),
    pricing_schedule: Some(&PEAK_HOURS),
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

/// Peak hours double every rate, and `models/deepseek.toml` quotes the off-peak
/// ones. The weekend stays off-peak around the clock.
/// <https://api-docs.deepseek.com/quick_start/pricing/>
pub(crate) const PEAK_HOURS: PricingSchedule =
    PricingSchedule::new(PEAK_WINDOWS, PEAK_MULTIPLIER).weekdays_only();

const PEAK_WINDOWS: &[PricingWindow] = &[PricingWindow::hours(1, 4), PricingWindow::hours(6, 10)];
const PEAK_MULTIPLIER: f64 = 2.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::ProviderRegistry;

    /// The hours, days and surcharge as the pricing page states them.
    const PUBLISHED_PEAK_HOURS: &str = "2x during 01:00-04:00, 06:00-10:00 UTC, Mon-Fri";

    /// A schedule that never got hooked up to the spec looks exactly like
    /// off-peak all day, so the assert goes through the registry the biller
    /// reads. Drift on either side bills every DeepSeek turn at the wrong rate.
    #[test]
    fn the_manifest_bills_the_published_peak_hours() {
        let schedule = ProviderRegistry::get(SLUG)
            .expect("deepseek is a builtin")
            .pricing_schedule
            .expect("deepseek bills by the clock");
        assert_eq!(schedule.to_string(), PUBLISHED_PEAK_HOURS);
    }
}

/// The recorded cases the bundled `deepseek` plugin replays.
///
/// Every case was recorded while the bespoke `impl Provider` this module used
/// to hold was still here. That impl wrote `thinking` first and asked for an
/// effort string second, where the declared path applies the effort in the
/// codec before the hook that writes `thinking` runs. Whether that swap shows
/// on the wire is a question for the goldens, which is why every thinking mode
/// has one. The impl is gone and the artifacts are not.
#[cfg(any(test, feature = "test-support"))]
pub mod fixtures {
    use serde_json::json;

    use crate::model::Model;
    use crate::providers::replay::Fixture;
    use crate::test_support::Canned;
    use crate::types::ContentBlock;
    use crate::{Effort, Message, Role, ThinkingConfig};

    pub const FLASH_SPEC: &str = "deepseek/deepseek-flash";
    /// The one id outside the V4 thinking protocol, and not in the curated
    /// table, so it also stands for any id a DeepSeek-based custom provider is
    /// pointed at.
    pub const REASONER_SPEC: &str = "deepseek/deepseek-reasoner";
    /// DeepSeek accepts `max` and nothing else, so a level below it is what
    /// proves the dialect snaps rather than passes through.
    pub const EFFORT: Effort = Effort::High;

    const UNKNOWN_MODEL: &str = "the model spec did not resolve";

    const FIRST_ASK: &str = "read a.txt";
    const KEPT_REASONING: &str = "a.txt first";
    const REPLY: &str = "on it";
    const TOOL_ID: &str = "call_1";
    const TOOL_NAME: &str = "read";
    const TOOL_INPUT_PATH: &str = "a.txt";
    const TOOL_OUTPUT: &str = "contents of a.txt";
    const FOLLOW_UP: &str = "now read b.txt";

    const BALANCE_UNAUTHORIZED_BODY: &str = r#"{"error":{"message":"Authentication Fails, Your api key: ****abcd is invalid","type":"authentication_error","param":null,"code":"invalid_request_error"}}"#;
    const BALANCE_BODY: &str = r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"12.34","granted_balance":"2.00","topped_up_balance":"10.34"},{"currency":"CNY","total_balance":"88.00","granted_balance":"0.00","topped_up_balance":"88.00"}]}"#;

    const SUCCESS_TRANSCRIPT: &str = r#"data: {"choices":[{"delta":{"reasoning_content":"weighing the options"}}]}

data: {"choices":[{"delta":{"content":"Hello"}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_2","function":{"name":"read","arguments":"{\"path\":"}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"b.txt\"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":5,"prompt_cache_hit_tokens":4}}

data: [DONE]

"#;

    pub const SUCCESS_SCRIPT: &[Canned] = &[Canned::sse(SUCCESS_TRANSCRIPT)];

    pub const SUCCESS: Fixture = Fixture {
        name: "success",
        script: SUCCESS_SCRIPT,
        thinking: ThinkingConfig::Effort(EFFORT),
        session: None,
    };
    /// The mode the toggle has to spell out, since DeepSeek reasons unless it
    /// is told not to.
    pub const THINKING_OFF: Fixture = Fixture {
        name: "thinking_off",
        script: SUCCESS_SCRIPT,
        thinking: ThinkingConfig::Off,
        session: None,
    };
    /// The quiet one: [`crate::dialect::DEEPSEEK`] declares no adaptive
    /// string, so thinking is switched on while `reasoning_effort` stays off
    /// the wire and the API picks its own depth.
    pub const THINKING_ADAPTIVE: Fixture = Fixture {
        name: "thinking_adaptive",
        script: SUCCESS_SCRIPT,
        thinking: ThinkingConfig::Adaptive,
        session: None,
    };
    pub const BALANCE: Fixture = Fixture {
        name: "user_balance",
        script: &[Canned::json(200, BALANCE_BODY)],
        thinking: ThinkingConfig::Off,
        session: None,
    };
    /// A refused balance request. The plugin must fail with the `Api` error the
    /// bespoke impl failed with, so it cannot pass by calling it a broken hook.
    pub const USER_BALANCE_UNAUTHORIZED: Fixture = Fixture {
        name: "user_balance_unauthorized",
        script: &[Canned::json(401, BALANCE_UNAUTHORIZED_BODY)],
        thinking: ThinkingConfig::Off,
        session: None,
    };

    /// The padding cases, which every authoring replays against the same
    /// recording. Only the request in front of it changes, so the name is what
    /// tells the goldens apart.
    pub fn padding(name: &'static str) -> Fixture {
        Fixture {
            name,
            script: SUCCESS_SCRIPT,
            thinking: ThinkingConfig::Effort(EFFORT),
            session: None,
        }
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        }
    }

    /// A history with one of each turn the padding has an opinion about: an
    /// assistant reply that already carries reasoning, an assistant turn that
    /// is nothing but a tool call, and the tool result and user turns that must
    /// come back untouched.
    pub fn history() -> Vec<Message> {
        vec![
            Message::user(FIRST_ASK.to_owned()),
            assistant(vec![
                ContentBlock::Thinking {
                    thinking: KEPT_REASONING.to_owned(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: REPLY.to_owned(),
                },
            ]),
            assistant(vec![ContentBlock::ToolUse {
                id: TOOL_ID.to_owned(),
                name: TOOL_NAME.to_owned(),
                input: json!({"path": TOOL_INPUT_PATH}),
                thought_signature: None,
            }]),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: TOOL_ID.to_owned(),
                    content: TOOL_OUTPUT.to_owned(),
                    is_error: false,
                }],
                ..Default::default()
            },
            Message::user(FOLLOW_UP.to_owned()),
        ]
    }

    pub fn model(spec: &str) -> Model {
        Model::from_spec(spec).expect(UNKNOWN_MODEL)
    }
}
