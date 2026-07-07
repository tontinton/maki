use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_config::providers::{BuiltInProvider, Protocol, ProviderPlan};
use serde::Deserialize;
use serde_json::Value;
use tracing::warn;

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::{BoxFuture, Provider};
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::{
    AgentError, EffortScale, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse,
    UsageLimit,
};

use super::{KeyPool, ResolvedAuth};

static CONFIG_STANDARD: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "ZHIPU_API_KEY",
    base_url: "https://api.z.ai/api/paas/v4",
    max_tokens_field: "max_tokens",
    include_stream_usage: false,
    provider_name: "Z.AI",
};

const QUOTA_LIMIT_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";

#[derive(Deserialize)]
struct QuotaResponse {
    data: QuotaData,
}

#[derive(Deserialize, Default)]
struct QuotaData {
    #[serde(default)]
    limits: Vec<QuotaLimit>,
    #[serde(default)]
    level: Option<String>,
}

#[derive(Deserialize)]
struct QuotaLimit {
    #[serde(rename = "type")]
    kind: String,
    unit: u32,
    percentage: u32,
    #[serde(default, rename = "nextResetTime")]
    next_reset_time: Option<u64>,
}

fn quota_label(kind: &str, unit: u32) -> String {
    match (kind, unit) {
        ("TOKENS_LIMIT", 3) => "5-hour tokens".into(),
        ("TOKENS_LIMIT", 6) => "Weekly tokens".into(),
        ("TIME_LIMIT", _) => "Subscription time".into(),
        _ => format!("{kind} #{unit}"),
    }
}

impl From<QuotaResponse> for ProviderUsage {
    fn from(resp: QuotaResponse) -> Self {
        ProviderUsage {
            plan: resp.data.level,
            limits: resp
                .data
                .limits
                .into_iter()
                .map(|l| UsageLimit {
                    label: quota_label(&l.kind, l.unit),
                    percentage: l.percentage,
                    reset_at: l.next_reset_time,
                    detail: None,
                })
                .collect(),
        }
    }
}

inventory::submit!(BuiltInProvider {
    slug: "zai",
    display_name: "Z.AI",
    protocol: Protocol::Openai,
    default_base_url: "https://api.z.ai/api/paas/v4",
    default_api_key_env: "ZHIPU_API_KEY",
    default_model: "zai/glm-5.1",
    plans: Some(&[
        (
            "standard",
            ProviderPlan {
                display_name: "Pay-as-you-go",
                base_url: "https://api.z.ai/api/paas/v4",
                default_model: Some("zai/glm-5.1"),
                login_url: None,
            }
        ),
        (
            "coding",
            ProviderPlan {
                display_name: "Coding plan",
                base_url: "https://api.z.ai/api/coding/paas/v4",
                default_model: Some("zai/glm-5-code"),
                login_url: None,
            }
        ),
    ]),
    login_url: Some("https://z.ai/manage-apikey/apikey-list"),
    needs_url: false,
});

pub(crate) fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["glm-5-code"],
            tier: ModelTier::Strong,
            family: ModelFamily::Glm,
            vision: false,
            default: true,
            pricing: ModelPricing {
                input: 1.20,
                output: 5.00,
                cache_write: 0.00,
                cache_read: 0.30,
                fast: None,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["glm-5.2"],
            tier: ModelTier::Strong,
            family: ModelFamily::Glm,
            vision: false,
            default: false,
            pricing: ModelPricing {
                input: 1.00,
                output: 3.20,
                cache_write: 0.00,
                cache_read: 0.20,
                fast: None,
            },
            max_output_tokens: 131072,
            context_window: 1_000_000,
        },
        ModelEntry {
            prefixes: &["glm-5.1", "glm-5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Glm,
            vision: false,
            default: false,
            pricing: ModelPricing {
                input: 1.00,
                output: 3.20,
                cache_write: 0.00,
                cache_read: 0.20,
                fast: None,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["glm-4.7-flash"],
            tier: ModelTier::Weak,
            family: ModelFamily::Glm,
            vision: false,
            default: true,
            pricing: ModelPricing {
                input: 0.00,
                output: 0.00,
                cache_write: 0.00,
                cache_read: 0.00,
                fast: None,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["glm-4.7", "glm-4.6"],
            tier: ModelTier::Medium,
            family: ModelFamily::Glm,
            vision: false,
            default: true,
            pricing: ModelPricing {
                input: 0.60,
                output: 2.20,
                cache_write: 0.00,
                cache_read: 0.11,
                fast: None,
            },
            max_output_tokens: 131072,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["glm-4.5-flash"],
            tier: ModelTier::Weak,
            family: ModelFamily::Glm,
            vision: false,
            default: false,
            pricing: ModelPricing {
                input: 0.00,
                output: 0.00,
                cache_write: 0.00,
                cache_read: 0.00,
                fast: None,
            },
            max_output_tokens: 98304,
            context_window: 131_072,
        },
        ModelEntry {
            prefixes: &["glm-4.5-air"],
            tier: ModelTier::Weak,
            family: ModelFamily::Glm,
            vision: false,
            default: false,
            pricing: ModelPricing {
                input: 0.20,
                output: 1.10,
                cache_write: 0.00,
                cache_read: 0.03,
                fast: None,
            },
            max_output_tokens: 98304,
            context_window: 131_072,
        },
        ModelEntry {
            prefixes: &["glm-4.5"],
            tier: ModelTier::Medium,
            family: ModelFamily::Glm,
            vision: false,
            default: false,
            pricing: ModelPricing {
                input: 0.60,
                output: 2.20,
                cache_write: 0.00,
                cache_read: 0.11,
                fast: None,
            },
            max_output_tokens: 98304,
            context_window: 131_072,
        },
    ]
}

pub struct Zai {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl Zai {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("zai", CONFIG_STANDARD.api_key_env)?;
        let mut auth = ResolvedAuth::bearer(pool.current());
        let provider_config = maki_config::providers::ProvidersConfig::load();
        if let Some(url) =
            maki_config::providers::resolve_base_url("zai", provider_config.get("zai"))
        {
            auth.base_url = Some(url);
        }
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG_STANDARD, timeouts),
            auth: Arc::new(Mutex::new(auth)),
            key_pool: Some(pool),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG_STANDARD, timeouts),
            auth,
            key_pool: None,
            system_prefix: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }
}

impl Provider for Zai {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body = self.compat.build_body(model, messages, system, tools);
            if model.supports_thinking() {
                opts.thinking
                    .apply_reasoning_effort(&mut body, EffortScale::Glm);
            }
            match self
                .compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
            {
                Err(AgentError::Api { status, message })
                    if (status == 429 || status >= 500)
                        && (message.contains("1113") || message.contains("nsufficien")) =>
                {
                    warn!(status, "insufficient funds, bailing out");
                    Err(AgentError::Api {
                        status: 402,
                        message,
                    })
                }
                result => result,
            }
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            self.compat.do_list_models(&auth).await
        })
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let body = self.compat.get_text(&auth, QUOTA_LIMIT_URL).await?;
            let parsed: QuotaResponse = serde_json::from_str(&body)?;
            Ok(Some(parsed.into()))
        })
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            Ok(self
                .key_pool
                .as_ref()
                .is_some_and(|p| p.rotate_auth(&self.auth, ResolvedAuth::bearer)))
        })
    }

    fn adjust_model(&self, model: &mut Model) {
        adjust_model(model);
    }
}

fn adjust_model(model: &mut Model) {
    if model.id.starts_with("glm-5.2") {
        model.supports_thinking_override = Some(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const SAMPLE_BODY: &str = r#"{"code":200,"data":{"limits":[
        {"type":"TOKENS_LIMIT","unit":3,"percentage":16,"nextResetTime":1777819631597},
        {"type":"TOKENS_LIMIT","unit":6,"percentage":4,"nextResetTime":1778262784969},
        {"type":"TIME_LIMIT","unit":5,"percentage":0,"nextResetTime":1780336384978}
    ],"level":"lite"}}"#;

    #[test_case("zai/glm-5.2", true ; "glm_5_2_supports_thinking")]
    #[test_case("zai/glm-5.1", false ; "glm_5_1_no_thinking")]
    #[test_case("zai/glm-4.7", false ; "glm_4_7_no_thinking")]
    fn adjust_model_sets_thinking_support(spec: &str, expected: bool) {
        let mut model = Model::from_spec(spec).unwrap();
        adjust_model(&mut model);
        assert_eq!(model.supports_thinking(), expected);
    }

    #[test]
    fn parse_quota_response() {
        let parsed: QuotaResponse = serde_json::from_str(SAMPLE_BODY).unwrap();
        let usage: ProviderUsage = parsed.into();
        assert_eq!(usage.plan.as_deref(), Some("lite"));
        assert_eq!(usage.limits.len(), 3);
        assert_eq!(usage.limits[0].label, "5-hour tokens");
        assert_eq!(usage.limits[0].percentage, 16);
        assert_eq!(usage.limits[0].reset_at, Some(1777819631597));
        assert_eq!(usage.limits[1].label, "Weekly tokens");
        assert_eq!(usage.limits[2].label, "Subscription time");
        assert_eq!(usage.limits[2].reset_at, Some(1780336384978));
    }

    #[test]
    fn parse_quota_unknown_unit_falls_back() {
        let body = r#"{"code":200,"data":{"limits":[
            {"type":"TOKENS_LIMIT","unit":9,"percentage":50}
        ]}}"#;
        let parsed: QuotaResponse = serde_json::from_str(body).unwrap();
        let usage: ProviderUsage = parsed.into();
        assert!(usage.plan.is_none());
        assert_eq!(usage.limits[0].label, "TOKENS_LIMIT #9");
        assert_eq!(usage.limits[0].percentage, 50);
        assert_eq!(usage.limits[0].reset_at, None);
    }
}
