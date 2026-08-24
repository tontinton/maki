//! Command Code, as two providers over one account.
//!
//! [`credits`] is the metered pay-as-you-go Provider plan: an ordinary
//! OpenAI-compatible endpoint under `/provider/v1`, authenticated with a
//! pasted API key, so the shared compat layer does all the work.
//!
//! [`plan`] is the token plans (GOAT / Pro / Max / Team). Those do not speak
//! chat-completions at all: they use a custom SSE protocol at
//! `POST /alpha/generate` and are authenticated through the browser handshake
//! in [`auth`], since a plan key is minted in the studio rather than pasted.
//!
//! Both bill the same account and accept the same key, and both read their
//! per-model reasoning and vision support from [`CATALOG`], because the shared
//! `/provider/v1/models` catalog exposes neither.

use maki_storage::sessions::Effort;
use maki_storage::sessions::Effort::{High, Low, Max, Medium, XHigh};
use serde_json::Value;

use crate::model::{Model, ModelEntry, ModelInfo, ThinkingSupport};
use crate::types::EffortDialect;
use crate::{AgentError, ThinkingConfig};

use super::{KeyPool, ResolvedAuth};

pub mod auth;
mod credits;
mod plan;

pub(crate) use credits::CommandCodeCredits;
pub(crate) use plan::CommandCode;

pub(crate) const PLAN_SLUG: &str = "command-code";
pub(crate) const CREDITS_SLUG: &str = "command-code-credits";
pub(crate) const BASE_URL: &str = "https://api.commandcode.ai";
/// The metered endpoint is the OpenAI-compatible surface under the same host.
pub(crate) const CREDITS_BASE_URL: &str = "https://api.commandcode.ai/provider/v1";
pub(crate) const ENV_VAR: &str = "COMMAND_CODE_API_KEY";
/// The generate endpoint's own ceiling, independent of the model window.
pub(crate) const MAX_GENERATE_TOKENS: u32 = 64_000;

/// Empty on purpose for both slugs: the catalog is fetched from
/// `/provider/v1/models`, and a token plan bills against the subscription, so
/// a static per-token table here would only be a second source of truth.
pub(crate) const fn models() -> &'static [ModelEntry] {
    &[]
}

const FULL: &[Effort] = &[Low, Medium, High, XHigh, Max];
const TO_XHIGH: &[Effort] = &[Low, Medium, High, XHigh];
const TO_HIGH: &[Effort] = &[Low, Medium, High];
const HIGH_MAX: &[Effort] = &[High, Max];
const HIGH_XHIGH: &[Effort] = &[High, XHigh];
const NONE: &[Effort] = &[];

/// `(model id, accepted reasoning efforts, accepts image input)`.
///
/// `/provider/v1/models` returns only id/name/context_length, so reasoning and
/// vision have to come from somewhere: this is a snapshot of the
/// command-code@1.15.1 bundled catalog. An id missing here is treated as
/// text-only with provider-chosen reasoning depth, which is what the CLI does
/// too, so a newly released model degrades instead of erroring.
///
/// ponytail: hand-maintained snapshot. Refresh from
/// <https://commandcode.ai/docs/resources/pricing-limits> when models land; if
/// the catalog endpoint ever exposes these fields, delete the table.
const CATALOG: &[(&str, &[Effort], bool)] = &[
    ("MiniMaxAI/MiniMax-M3", NONE, true),
    ("Qwen/Qwen3.6-Plus", NONE, true),
    ("Qwen/Qwen3.7-Flash", NONE, true),
    ("Qwen/Qwen3.7-Plus", NONE, true),
    ("Qwen/Qwen3.8-Max", &[Low, Medium, XHigh], true),
    ("claude-fable-5", FULL, true),
    ("claude-haiku-4-5-20251001", NONE, true),
    ("claude-opus-4-7", FULL, true),
    ("claude-opus-4-8", FULL, true),
    ("claude-opus-5", FULL, true),
    ("claude-sonnet-4-6", FULL, true),
    ("claude-sonnet-5", FULL, true),
    ("deepseek/deepseek-v4-flash", HIGH_MAX, false),
    ("deepseek/deepseek-v4-pro", HIGH_MAX, false),
    ("google/gemini-3.1-flash-lite", TO_HIGH, true),
    ("google/gemini-3.5-flash", TO_HIGH, true),
    ("google/gemini-3.5-flash-lite", TO_HIGH, true),
    ("google/gemini-3.6-flash", TO_HIGH, true),
    ("gpt-5.3-codex", TO_XHIGH, true),
    ("gpt-5.4", TO_XHIGH, true),
    ("gpt-5.4-mini", TO_HIGH, true),
    ("gpt-5.5", TO_XHIGH, true),
    ("gpt-5.6-luna", FULL, true),
    ("gpt-5.6-sol", FULL, true),
    ("gpt-5.6-terra", FULL, true),
    ("meta/muse-spark-1.1", NONE, true),
    ("meta/muse-spark-1.2", NONE, true),
    ("meta/muse-spark-1.2-contributor", NONE, true),
    ("moonshotai/Kimi-K2.5", NONE, true),
    ("moonshotai/Kimi-K2.6", NONE, true),
    ("moonshotai/Kimi-K2.7-Code", NONE, true),
    ("moonshotai/Kimi-K2.7-Code-Highspeed", NONE, true),
    ("moonshotai/Kimi-K3", NONE, true),
    ("sakana/fugu-ultra", HIGH_XHIGH, true),
    ("stepfun/Step-3.7-Flash", NONE, true),
    ("thinkingmachines/inkling", NONE, true),
    ("thinkingmachines/inkling-small", NONE, true),
    ("xai/grok-4.5", TO_HIGH, true),
    ("xiaomi/mimo-v2.5", NONE, true),
    ("zai-org/GLM-5.2", HIGH_MAX, false),
];

pub(super) fn catalog_entry(
    model_id: &str,
) -> Option<&'static (&'static str, &'static [Effort], bool)> {
    CATALOG.iter().find(|(id, _, _)| *id == model_id)
}

/// `None` means send no `reasoning_effort` and let Command Code pick, which is
/// also what an unknown model gets.
pub(super) fn reasoning_effort(model: &Model, thinking: ThinkingConfig) -> Option<&'static str> {
    let (_, efforts, _) = catalog_entry(&model.id)?;
    if efforts.is_empty() {
        return None;
    }
    thinking.effort_str(
        &EffortDialect {
            supported: efforts,
            // Command Code has no adaptive level and no explicit opt-out
            // string: both mean "omit the field".
            adaptive: None,
            off: None,
        },
        model,
    )
}

/// Seeds the capabilities discovery has not fetched yet. Until it runs, the
/// Generic manifest answers "no vision, thinking on everything", which
/// silently strips images from the first turn on a vision model. Unknown ids
/// fall through untouched so discovery can still fill them in.
pub(super) fn adjust_model(model: &mut Model) {
    let Some((_, efforts, vision)) = catalog_entry(&model.id) else {
        return;
    };
    model.supports_vision_override = Some(*vision);
    model.thinking_override = Some(if efforts.is_empty() {
        ThinkingSupport::No
    } else {
        ThinkingSupport::Yes
    });
}

/// The reasoning efforts the endpoint accepts for this model, as a dialect the
/// shared effort plumbing can snap against. `None` means send nothing and let
/// Command Code choose, which is also what an unknown model gets.
pub(super) fn effort_dialect(model_id: &str) -> Option<EffortDialect<'static>> {
    let (_, efforts, _) = catalog_entry(model_id)?;
    if efforts.is_empty() {
        return None;
    }
    Some(EffortDialect {
        supported: efforts,
        // Command Code has no adaptive level and no explicit opt-out string:
        // both mean "omit the field".
        adaptive: None,
        off: None,
    })
}

/// Credential files written by the Command Code CLI and by pi/omp hosts. maki's
/// own `KeyPool` (env, `maki auth login`, providers.toml) is tried first; this
/// is the fallback that makes an existing CLI login just work.
fn key_from_cli_files() -> Option<String> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let paths = [
        home.join(".commandcode/auth.json"),
        home.join(".omp/agent/auth.json"),
        home.join(".pi/agent/auth.json"),
    ];
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(root) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let direct = ["apiKey", "commandcode"]
            .iter()
            .find_map(|k| root.get(*k)?.as_str());
        if let Some(key) = direct {
            return Some(key.to_string());
        }
        // `{"command-code": {"type":"api","key":"..."}}` from the CLI, or the
        // same shape with `type: "oauth"` and an `access` token.
        let nested = ["commandcode", "command-code"].iter().find_map(|k| {
            let record = root.get(*k)?;
            record.get("key").or_else(|| record.get("access"))?.as_str()
        });
        if let Some(key) = nested {
            return Some(key.to_string());
        }
    }
    None
}

/// One Command Code account issues one API key, and both endpoints accept it,
/// so a login under either slug authenticates the other. Without this a plan
/// subscriber who also wants metered credits would log in twice for one key.
fn sibling_key(slug: &str) -> Option<String> {
    let sibling = if slug == PLAN_SLUG {
        CREDITS_SLUG
    } else {
        PLAN_SLUG
    };
    let dir = maki_storage::StateDir::resolve().ok()?;
    maki_storage::auth::load_provider_credentials(&dir, sibling).map(|c| c.api_key)
}

pub(super) fn resolve_key_pool(slug: &str) -> Result<KeyPool, AgentError> {
    match KeyPool::resolve(slug, ENV_VAR) {
        Ok(pool) => Ok(pool),
        Err(e) => sibling_key(slug)
            .or_else(key_from_cli_files)
            .map_or(Err(e), |key| Ok(KeyPool::from_keys(vec![key]))),
    }
}

pub(super) fn resolve_auth_from_key(key: &str, base_url: Option<String>) -> ResolvedAuth {
    let mut auth = ResolvedAuth::bearer(key);
    auth.base_url = base_url;
    auth
}

/// One row of `GET /provider/v1/models`, shared because both providers read
/// the same catalog and both need the [`CATALOG`] capabilities folded in.
pub(super) fn parse_model(m: &Value) -> Option<ModelInfo> {
    let id = m["id"].as_str()?;
    let entry = catalog_entry(id);
    Some(ModelInfo {
        context_window: m["context_length"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok()),
        // The catalog exposes no output ceiling, and the context window is not
        // one: an 8k model would end up spending its whole window on output.
        // Left unset so `ProviderManifest::fallback_max_output` decides.
        max_output_tokens: None,
        supports_thinking: entry.map(|(_, efforts, _)| !efforts.is_empty()),
        supports_vision: entry.map(|(_, _, vision)| *vision),
        ..ModelInfo::id_only(id.to_string())
    })
}

/// The whole catalog response, for [`plan`], which does not go through the
/// compat layer's `fetch_and_parse_models`.
pub(super) fn parse_models(body: &str) -> Result<Vec<ModelInfo>, AgentError> {
    let body: Value = serde_json::from_str(body)?;
    let mut infos: Vec<ModelInfo> = body["data"]
        .as_array()
        .map(|arr| arr.iter().filter_map(parse_model).collect())
        .unwrap_or_default();
    infos.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(infos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelFamily, ModelPricing, ModelTier};
    use maki_storage::sessions::Effort::{High, Low, XHigh};
    use std::sync::Arc;

    pub(super) fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            provider: Arc::from(PLAN_SLUG),
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            supports_tool_examples_override: None,
            thinking_override: None,
            supports_vision_override: None,
            pricing: ModelPricing::default(),
            max_output_tokens: Some(200_000),
            context_window: 400_000,
            discovered_free: false,
            thinking_fields: None,
        }
    }

    #[test]
    fn effort_snaps_to_what_the_model_accepts() {
        // deepseek accepts only high/max, so Low must not go out as "low".
        assert_eq!(
            reasoning_effort(
                &model("deepseek/deepseek-v4-pro"),
                ThinkingConfig::Effort(Low)
            ),
            Some("high"),
        );
        assert_eq!(
            reasoning_effort(&model("claude-opus-5"), ThinkingConfig::Effort(XHigh)),
            Some("xhigh"),
        );
        // Non-reasoning and unknown models send nothing at all.
        assert_eq!(
            reasoning_effort(&model("moonshotai/Kimi-K3"), ThinkingConfig::Effort(High)),
            None,
        );
        assert_eq!(
            reasoning_effort(&model("brand/new-model"), ThinkingConfig::Effort(High)),
            None,
        );
        assert_eq!(
            reasoning_effort(&model("claude-opus-5"), ThinkingConfig::Off),
            None,
        );
    }

    #[test]
    fn catalog_capabilities_apply_before_discovery_warms() {
        let mut vision_model = model("claude-opus-5");
        adjust_model(&mut vision_model);
        assert!(vision_model.supports_vision());
        assert!(vision_model.supports_thinking());

        let mut plain = model("moonshotai/Kimi-K3");
        adjust_model(&mut plain);
        assert!(plain.supports_vision());
        assert!(!plain.supports_thinking());

        // Unknown ids stay untouched so discovery can still fill them in.
        let mut unknown = model("brand/new-model");
        adjust_model(&mut unknown);
        assert!(unknown.supports_vision_override.is_none());
        assert!(unknown.thinking_override.is_none());
    }

    #[test]
    fn parse_models_folds_in_catalog_capabilities() {
        let body = r#"{"data":[
            {"id":"claude-opus-5","name":"Opus","context_length":400000},
            {"id":"moonshotai/Kimi-K3","name":"K3","context_length":200000},
            {"id":"brand/new-model","name":"New","context_length":8000}
        ]}"#;
        let models = parse_models(body).unwrap();
        assert_eq!(models.len(), 3);
        // Sorted by id, and the context window never leaks into the output cap.
        assert_eq!(models[0].id, "brand/new-model");
        assert_eq!(models[0].context_window, Some(8_000));
        assert_eq!(models[0].max_output_tokens, None);
        assert_eq!(models[0].supports_vision, None);

        let opus = models.iter().find(|m| m.id == "claude-opus-5").unwrap();
        assert_eq!(opus.max_output_tokens, None);
        assert_eq!(opus.supports_vision, Some(true));
        assert_eq!(opus.supports_thinking, Some(true));

        let kimi = models.iter().find(|m| m.id.contains("Kimi")).unwrap();
        assert_eq!(kimi.supports_thinking, Some(false));
    }
}
