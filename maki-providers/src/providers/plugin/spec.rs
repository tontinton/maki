//! Turns a declaration into a spec row. Model lookup, pricing, Aperture
//! routing, login and the docs all take a `&'static ProviderSpec`, and this is
//! what lets them treat a plugin provider like a compiled-in one.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use maki_config::providers::{BuiltInProvider, Protocol, ProviderPlan};

use crate::manifest::check_rows;
use crate::model::ModelEntry;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GENERIC_DISCOVERY_NOTE, GeneratedDocs,
    NO_CURATED_MODELS, ProviderSpec,
};

use super::{ProviderDecl, Target};

/// What a codec-backed provider assumes about a model nobody described, the
/// same conservative limits the other bring-your-own-endpoint providers use.
pub(super) const UNKNOWN_MODEL_MAX_OUTPUT: u32 = 16_384;
pub(super) const UNKNOWN_MODEL_CONTEXT_WINDOW: u32 = 128_000;

/// Rows must be `&'static`, so they are leaked. Each distinct declaration
/// leaks once and later loads reuse it, so a `/reload` leaks nothing and only
/// an edited declaration leaks again. The key is the declaration's JSON, which
/// is everything a row is built from.
static BUILT: LazyLock<Mutex<HashMap<String, Built>>> = LazyLock::new(Mutex::default);

#[derive(Clone)]
pub(super) struct Built {
    pub(super) spec: &'static ProviderSpec,
    /// Only with an `api_key_env`, since this row is how `maki auth login`
    /// knows to ask for a key.
    pub(super) login: Option<&'static BuiltInProvider>,
}

/// The author's model rows are checked before anything leaks, so a broken
/// declaration costs no memory.
pub(super) fn build(decl: &ProviderDecl, target: Target) -> Result<Built, String> {
    let key = serde_json::to_string(decl).expect("a declaration is plain data");
    let mut built = BUILT.lock().unwrap();
    if let Some(cached) = built.get(&key) {
        return Ok(cached.clone());
    }
    let models = declared_models(decl, target)?;
    let spec = leak(spec_row(decl, target, models));
    let row = Built {
        spec,
        login: decl
            .api_key_env
            .is_some()
            .then(|| leak(login_row(decl, target, spec))),
    };
    built.insert(key, row.clone());
    Ok(row)
}

/// Family and thinking support come from the native provider behind the codec
/// or base. For curation and limits a `base` lends its whole provider, but a
/// codec lends only its wire: its native row describes that vendor's own
/// models, not whatever the endpoint serves, so discovery assigns tiers and an
/// undescribed model gets safe limits.
fn spec_row(decl: &ProviderDecl, target: Target, models: &'static [ModelEntry]) -> ProviderSpec {
    let native = target.spec();
    let (accepts_arbitrary_models, max_output, context_window) = match target {
        Target::Base(spec) => (
            spec.accepts_arbitrary_models,
            spec.fallback_max_output,
            spec.fallback_context_window,
        ),
        Target::Codec(_) => (
            true,
            Some(UNKNOWN_MODEL_MAX_OUTPUT),
            UNKNOWN_MODEL_CONTEXT_WINDOW,
        ),
    };
    ProviderSpec {
        slug: leak_str(&decl.slug),
        display_name: leak_str(&decl.display_name),
        api_key_env: decl.api_key_env.as_deref().map_or("", leak_str),
        family: decl.family.unwrap_or(native.family),
        supports_thinking: native.supports_thinking,
        supports_deferred_tools: false,
        accepts_arbitrary_models: decl
            .accepts_arbitrary_models
            .unwrap_or(accepts_arbitrary_models),
        fallback_max_output: decl.max_output_tokens.unwrap_or(max_output),
        fallback_context_window: decl.context_window.unwrap_or(context_window),
        models_toml: NO_CURATED_MODELS,
        pricing_schedule: decl.pricing_schedule.clone().map(leak),
        build: Build::Declared(models),
        aperture: decl.aperture.as_ref().map(|route| ApertureRoute {
            path_prefix: leak_str(&route.path_prefix),
        }),
        login: None,
        docs: GeneratedDocs {
            api_urls: leak_slice(decl.base_url.iter().map(|url| leak_str(url)).collect()),
            features: decl.docs.features.as_deref().map(leak_str),
            auth: AuthDoc::EnvVar,
            catalog: if models.is_empty() {
                CatalogDoc::Discovered(
                    decl.docs
                        .discovery_note
                        .as_deref()
                        .map_or(GENERIC_DISCOVERY_NOTE, leak_str),
                )
            } else {
                CatalogDoc::Table
            },
            trailing_notes: &[],
        },
    }
}

/// A limit the declaration states lands on every row that leaves its own out.
/// A limit nobody stated stays unset, so a listed model resolves it like an
/// unlisted one, through discovery, models.dev and only then the spec's
/// fallback. Listing a model just to price it must never move its limits.
///
/// Every tier needs a default, so a tier with nothing marked gets its first
/// row. A `base` lends its table under the declared rows, so a model the author
/// did not list keeps the base's price and limits. A codec lends nothing,
/// because its native table lists another provider's models.
fn declared_models(decl: &ProviderDecl, target: Target) -> Result<&'static [ModelEntry], String> {
    if let Target::Base(spec) = target
        && decl.models.is_empty()
    {
        return Ok(spec.models());
    }
    // Only what the author wrote is checked. A borrowed output cap larger than
    // a stated window is the agent's to clamp, not the author's mistake.
    check_rows(&decl.models)?;
    let mut rows = decl.models.clone();
    for row in &mut rows {
        if let Some(stated) = decl.max_output_tokens {
            row.max_output_tokens = row.max_output_tokens.or(stated);
        }
        row.context_window = row.context_window.or(decl.context_window);
    }
    for index in 0..rows.len() {
        let tier = rows[index].tier;
        if !rows.iter().any(|row| row.tier == tier && row.default) {
            rows[index].default = true;
        }
    }
    if let Target::Base(spec) = target {
        let lent = lent_rows(spec.models(), &rows);
        rows.extend(lent);
    }
    Ok(leak_slice(rows))
}

/// The base's rows minus what the declaration took over: a prefix it lists,
/// and the default of a tier it covers.
fn lent_rows(base: &[ModelEntry], declared: &[ModelEntry]) -> Vec<ModelEntry> {
    let taken = |prefix: &String| declared.iter().any(|row| row.prefixes.contains(prefix));
    base.iter()
        .filter_map(|row| {
            let mut row = row.clone();
            row.prefixes.retain(|prefix| !taken(prefix));
            row.default &= !declared.iter().any(|own| own.tier == row.tier);
            (!row.prefixes.is_empty()).then_some(row)
        })
        .collect()
}

/// Reuses the strings `spec` already leaked.
fn login_row(decl: &ProviderDecl, target: Target, spec: &'static ProviderSpec) -> BuiltInProvider {
    let slug = spec.slug;
    let model_spec =
        |id: &Option<String>| id.as_deref().map(|id| leak_str(&format!("{slug}/{id}")));
    let default_base_url = decl.base_url.as_deref().map_or("", leak_str);
    let plans: Vec<(&'static str, ProviderPlan)> = decl
        .plans
        .iter()
        .map(|plan| {
            let row = ProviderPlan {
                display_name: leak_str(&plan.display_name),
                base_url: plan.base_url.as_deref().map_or(default_base_url, leak_str),
                default_model: model_spec(&plan.default_model),
                login_url: plan.login_url.as_deref().map(leak_str),
            };
            (leak_str(&plan.key), row)
        })
        .collect();
    BuiltInProvider {
        slug,
        display_name: spec.display_name,
        protocol: match target {
            Target::Codec(protocol) => protocol,
            Target::Base(spec) => spec
                .login
                .as_ref()
                .map_or(Protocol::Openai, |login| login.protocol),
        },
        default_base_url,
        default_api_key_env: spec.api_key_env,
        default_model: model_spec(&decl.default_model),
        plans: (!plans.is_empty()).then(|| leak_slice(plans)),
        login_url: decl.login_url.as_deref().map(leak_str),
        needs_url: false,
    }
}

fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

fn leak_str(text: &str) -> &'static str {
    Box::leak(text.into())
}

fn leak_slice<T>(items: Vec<T>) -> &'static [T] {
    Box::leak(items.into_boxed_slice())
}
