//! Model registry with prefix-based lookup and token accounting.
//! Lookup is prefix-based: `claude-sonnet-4-20250514` matches the `claude-sonnet-4` entry,
//! so dated snapshots resolve without registry churn. `context_tokens()` sums input + output
//! + cache reads/writes because the context window limit applies to all of them combined.

use std::any::Any;
use std::fmt;
use std::ops::AddAssign;
use std::str::FromStr;
use std::sync::Arc;

use jiff::Timestamp;
use maki_config::ModelPolicy;
use maki_storage::sessions::{MIN_THINKING_BUDGET, StoredTokenUsage};
use serde::{Deserialize, Serialize};

use crate::manifest::{ManifestRegistry, ProviderManifest};
use crate::model_registry;
use crate::providers::{anthropic, custom, dynamic};
use crate::types::ThinkingFields;

const PER_MILLION: f64 = 1_000_000.0;

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("model must be in 'provider/model' format (e.g. anthropic/claude-sonnet-4-20250514)")]
    InvalidFormat,
    #[error("unsupported provider '{0}'")]
    UnsupportedProvider(String),
    #[error("unknown model '{0}'")]
    UnknownModel(String),
    #[error("invalid model tier '{0}' (expected: strong, medium, weak)")]
    InvalidTier(String),
    #[error("no allowed model for {0}/{1}")]
    NoAllowedModel(String, ModelTier),
    #[error("no default model for {0}/{1}")]
    NoDefault(String, ModelTier),
    #[error("model '{0}' is not allowed by provider model policy")]
    NotAllowed(String),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    /// Anthropic fast mode charges a premium that differs per model. `None`
    /// means the model has no fast tier, so asking for fast mode quietly falls
    /// back to standard rates instead of overcharging.
    #[serde(default)]
    pub fast: Option<FastPricing>,
    /// Set when the model's per-token cost is prepaid via a flat subscription
    /// (e.g. Claude Max through cliproxy), so the billed cost is always `$0`
    /// even though the rates above still reflect the provider's published
    /// list price. Holds the subscription's display name (e.g. `"Max"`),
    /// shown next to the list-price reference. `Arc<str>` because it is
    /// cloned into every turn's cost event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subsidised_by: Option<std::sync::Arc<str>>,
}

/// Metadata discovered at runtime from a provider's `/models` endpoint.
/// All fields optional -- most providers only return an ID.
#[derive(Debug, Clone, Default)]
pub struct ModelInfo {
    pub id: String,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub pricing: Option<ModelPricing>,
    pub supports_thinking: Option<bool>,
    pub supports_vision: Option<bool>,
    pub tier: Option<ModelTier>,
    /// Store of additional metadata from the provider.
    pub provider_info: Option<Arc<dyn Any + Send + Sync>>,
}

impl ModelInfo {
    pub fn id_only(id: String) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }
}

/// Cache rates are missing on purpose: Anthropic derives them from `input` with
/// the same multipliers it uses for standard pricing, so storing them would just
/// invite the two copies to drift apart.
#[derive(Debug, Clone, Deserialize)]
pub struct FastPricing {
    pub input: f64,
    pub output: f64,
}

impl ModelPricing {
    /// Per-token rates with no fast tier and no subsidy -- the shape of
    /// every static catalog entry. `const` so provider catalogs can call it;
    /// it also spares each entry from spelling out fields it never sets.
    pub const fn per_token(input: f64, output: f64, cache_write: f64, cache_read: f64) -> Self {
        Self {
            input,
            output,
            cache_write,
            cache_read,
            fast: None,
            subsidised_by: None,
        }
    }

    /// Like [`per_token`](Self::per_token), with a fast-tier price. A flat
    /// constructor rather than a `with_fast(self)` builder: consuming `self`
    /// in a `const fn` trips E0493 because `ModelPricing` carries drop glue.
    pub const fn per_token_with_fast(
        input: f64,
        output: f64,
        cache_write: f64,
        cache_read: f64,
        fast_input: f64,
        fast_output: f64,
    ) -> Self {
        Self {
            input,
            output,
            cache_write,
            cache_read,
            fast: Some(FastPricing {
                input: fast_input,
                output: fast_output,
            }),
            subsidised_by: None,
        }
    }

    pub const ZERO: Self = Self {
        input: 0.0,
        output: 0.0,
        cache_write: 0.0,
        cache_read: 0.0,
        fast: None,
        subsidised_by: None,
    };

    pub fn is_zero(&self) -> bool {
        self.input == 0.0 && self.output == 0.0 && self.cache_write == 0.0 && self.cache_read == 0.0
    }

    /// Cache multipliers Anthropic applies on top of the base input rate.
    const CACHE_WRITE_MULTIPLIER: f64 = 1.25;
    const CACHE_READ_MULTIPLIER: f64 = 0.10;

    /// Fast mode only ever quotes two rates, so its cache rates come off its own
    /// input rate rather than the standard one they no longer relate to.
    fn rates(&self, fast: bool) -> (f64, f64, f64, f64) {
        match &self.fast {
            Some(f) if fast => (
                f.input,
                f.output,
                f.input * Self::CACHE_WRITE_MULTIPLIER,
                f.input * Self::CACHE_READ_MULTIPLIER,
            ),
            _ => (self.input, self.output, self.cache_write, self.cache_read),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    Claude,
    Generic,
    Gemini,
    Glm,
    Gpt,
    Synthetic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    Weak,
    Medium,
    Strong,
    Compaction,
}

impl fmt::Display for ModelTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Weak => "weak",
            Self::Medium => "medium",
            Self::Strong => "strong",
            Self::Compaction => "compaction",
        })
    }
}

impl FromStr for ModelTier {
    type Err = ModelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "weak" => Ok(Self::Weak),
            "medium" => Ok(Self::Medium),
            "strong" => Ok(Self::Strong),
            "compaction" => Ok(Self::Compaction),
            other => Err(ModelError::InvalidTier(other.to_string())),
        }
    }
}

impl From<maki_config::providers::Tier> for ModelTier {
    fn from(t: maki_config::providers::Tier) -> Self {
        use maki_config::providers::Tier;
        match t {
            Tier::Weak => Self::Weak,
            Tier::Medium => Self::Medium,
            Tier::Strong => Self::Strong,
            Tier::Compaction => Self::Compaction,
        }
    }
}

#[derive(Debug)]
pub struct ModelEntry {
    pub prefixes: &'static [&'static str],
    pub tier: ModelTier,
    pub family: ModelFamily,
    /// Gates vision-only tools (`view_image`) and image blocks at request time.
    pub vision: bool,
    pub default: bool,
    pub pricing: ModelPricing,
    pub max_output_tokens: Option<u32>,
    pub context_window: u32,
}

pub(crate) fn lookup_entry<'a>(
    entries: &'a [ModelEntry],
    model_id: &str,
) -> Result<&'a ModelEntry, ModelError> {
    entries
        .iter()
        .flat_map(|e| e.prefixes.iter().map(move |p| (p, e)))
        .filter(|(p, _)| model_id.starts_with(*p))
        .max_by_key(|(p, _)| p.len())
        .map(|(_, e)| e)
        .ok_or_else(|| ModelError::UnknownModel(model_id.to_string()))
}

impl ModelFamily {
    pub fn supports_tool_examples(self) -> bool {
        match self {
            ModelFamily::Claude | ModelFamily::Gpt | ModelFamily::Synthetic => true,
            ModelFamily::Generic | ModelFamily::Gemini | ModelFamily::Glm => false,
        }
    }

    /// Fallback for models missing from the static tables; per-model truth
    /// lives in `ModelEntry::vision`.
    pub fn supports_vision(self) -> bool {
        matches!(self, Self::Claude | Self::Gpt | Self::Gemini)
    }
}

const FAST_PROVIDER: &str = "anthropic";

/// `Required` marks APIs that reject requests with thinking disabled;
/// [`crate::RequestOptions::clamped`] raises `Off` to minimal effort for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingSupport {
    No,
    Yes,
    Required,
}

impl ThinkingSupport {
    /// `requires` wins: an API that rejects thinking-off requests
    /// necessarily supports thinking.
    pub fn from_flags(supports: Option<bool>, requires: bool) -> Option<Self> {
        match (requires, supports) {
            (true, _) => Some(Self::Required),
            (false, Some(true)) => Some(Self::Yes),
            (false, Some(false)) => Some(Self::No),
            (false, None) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Model {
    pub id: String,
    pub provider: Arc<str>,
    pub tier: ModelTier,
    pub family: ModelFamily,
    pub supports_tool_examples_override: Option<bool>,
    /// Resolved thinking support, used by gateway providers (e.g. Aperture)
    /// that stream through a native provider chosen at runtime. `None` falls
    /// back to discovery, then the provider manifest.
    pub thinking_override: Option<ThinkingSupport>,
    pub supports_vision_override: Option<bool>,
    pub pricing: ModelPricing,
    /// Discovery reported an explicit all-zero price. Distinct from a zero
    /// `pricing`, which also covers "no price is known".
    pub discovered_free: bool,
    /// `None` when unknown, see [`ProviderKind::fallback_max_output`].
    pub max_output_tokens: Option<u32>,
    pub context_window: u32,
    pub thinking_fields: Option<Box<ThinkingFields>>,
}

impl Model {
    /// When no static entry matches (a freshly released model the table has not
    /// caught up to yet), fall back to the provider defaults so it still resolves.
    fn from_base(manifest: &ProviderManifest, slug: &str, model_id: &str) -> Self {
        let static_entry = lookup_entry(manifest.models, model_id).ok();
        let spec = format!("{slug}/{model_id}");
        // Discovery keys `known_models` by the builtin slug, so a dynamic or
        // custom slug reads positional tiers and metadata through its base.
        let discovered = model_registry::discovered(manifest.slug, model_id);
        let discovered = discovered.as_ref();
        let tier = model_registry::tier_for(&spec, manifest.slug, static_entry.map(|e| e.tier));
        let family = static_entry.map_or(manifest.family, |entry| entry.family);
        let discovered_pricing = discovered.and_then(|info| info.pricing.as_ref());
        let pricing = discovered_pricing
            .or_else(|| static_entry.map(|entry| &entry.pricing))
            .cloned()
            .unwrap_or_default();
        let max_output_tokens = discovered
            .and_then(|info| info.max_output_tokens)
            .or_else(|| static_entry.and_then(|entry| entry.max_output_tokens))
            .or(manifest.fallback_max_output);
        let context_window = discovered
            .and_then(|info| info.context_window)
            .or_else(|| anthropic::shared::long_context_window(model_id))
            .or_else(|| static_entry.map(|entry| entry.context_window))
            .unwrap_or(manifest.fallback_context_window);
        Self {
            id: model_id.to_string(),
            provider: Arc::from(slug),
            tier,
            family,
            supports_tool_examples_override: None,
            thinking_override: None,
            supports_vision_override: None,
            pricing,
            discovered_free: discovered_pricing.is_some_and(ModelPricing::is_zero),
            max_output_tokens,
            context_window,
            thinking_fields: None,
        }
    }

    /// Build a `Model` from a models.dev catalogue sub-provider (nvidia,
    /// fireworks, groq, ...). The slug is the catalogue sub-provider key, not a
    /// builtin; metadata is read once from the models.dev catalog and cached on
    /// the `Model` so `supports_thinking`/`supports_vision` do not need a live
    /// catalog lookup.
    fn from_catalog(
        slug: &str,
        model_id: &str,
        meta: crate::providers::catalog::CatalogMetaView,
    ) -> Self {
        Self {
            id: model_id.to_string(),
            provider: Arc::from(slug),
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            supports_tool_examples_override: None,
            thinking_override: ThinkingSupport::from_flags(Some(meta.supports_thinking), false),
            supports_vision_override: Some(meta.supports_vision),
            pricing: ModelPricing::per_token(
                meta.input_price,
                meta.output_price,
                meta.cache_write,
                meta.cache_read,
            ),
            discovered_free: false,
            max_output_tokens: Some(meta.output),
            context_window: meta.context,
            thinking_fields: None,
        }
    }

    pub fn supports_thinking(&self) -> bool {
        if let Some(thinking) = self.thinking_override {
            return thinking != ThinkingSupport::No;
        }
        // Discovery keys `known_models` by the builtin slug; resolve dynamic
        // and custom slugs through their base manifest before looking up.
        let Some(manifest) = ManifestRegistry::for_slug(&self.provider) else {
            return false;
        };
        model_registry::discovered(manifest.slug, &self.id)
            .and_then(|d| d.supports_thinking)
            .unwrap_or(manifest.supports_thinking)
    }

    pub fn requires_thinking(&self) -> bool {
        self.thinking_override == Some(ThinkingSupport::Required)
    }

    pub fn supports_vision(&self) -> bool {
        if let Some(vision) = self.supports_vision_override {
            return vision;
        }
        let manifest = ManifestRegistry::for_slug(&self.provider);
        manifest
            .and_then(|m| {
                model_registry::discovered(m.slug, &self.id).and_then(|d| d.supports_vision)
            })
            .or_else(|| {
                manifest
                    .and_then(|m| lookup_entry(m.models, &self.id).ok())
                    .map(|e| e.vision)
            })
            .unwrap_or_else(|| self.family.supports_vision())
    }

    pub fn supports_tool_examples(&self) -> bool {
        self.supports_tool_examples_override
            .unwrap_or_else(|| self.family.supports_tool_examples())
    }

    /// Half the output window, so the answer always has room after the
    /// thinking. `None` when the window is unknown: callers must then let
    /// budgets through unclamped. Providers cap further only where the API
    /// documents a hard limit (currently just Google).
    pub fn max_thinking_budget(&self) -> Option<u32> {
        self.max_output_tokens
            .map(|n| (n / 2).max(MIN_THINKING_BUDGET))
    }

    /// A model supports fast mode exactly when it carries fast-tier pricing, so
    /// capability and billing can never disagree. The provider gate keeps fast
    /// mode to Anthropic-based providers, resolved through the base manifest so
    /// oauth scripts keep it; Bedrock separately ignores `opts.fast` at request
    /// time.
    pub fn supports_fast(&self) -> bool {
        self.pricing.fast.is_some()
            && ManifestRegistry::for_slug(&self.provider).is_some_and(|m| m.slug == FAST_PROVIDER)
    }

    pub fn spec(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// What the provider charges right now, so it is only ever correct for a
    /// turn that just finished: under a
    /// [`PricingSchedule`](crate::pricing::PricingSchedule) the answer moves
    /// with the clock. Anything historical wants [`Self::list_cost`].
    ///
    /// `None` on an unpriced model (oauth, local), so callers can hide the cost
    /// instead of showing a misleading "$0.000".
    ///
    /// A bill the provider sent us is the whole answer, so it skips both the
    /// table and the surcharge: it is already the price at this hour, and we
    /// have one even for a model nothing ever quoted a rate for. Except a bill
    /// of zero, which is all a free model ever sends, and free has always shown
    /// no cost rather than "$0.000".
    pub fn billed_cost(&self, usage: &TokenUsage, fast: bool) -> Option<f64> {
        if self.pricing.subsidised_by.is_some() {
            // Prepaid via a flat subscription: the turn ran, but nothing was
            // billed per-token. Still require a real price table so an
            // unpriced model does not report a false "$0.000".
            return self.list_cost(usage, fast).map(|_| 0.0);
        }
        usage.cost.filter(|bill| *bill > 0.0).or_else(|| {
            let cost = self.list_cost(usage, fast)?;
            let schedule =
                ManifestRegistry::for_slug(&self.provider).and_then(|m| m.pricing_schedule);
            Some(schedule.map_or(cost, |s| cost * s.multiplier_at(Timestamp::now())))
        })
    }

    /// The quoted rates, with no wall-clock surcharge. Deterministic, which is
    /// what makes it right for re-pricing a session whose turns never recorded
    /// what they paid: the rate back then is unknown, and the table price is
    /// the honest guess.
    pub fn list_cost(&self, usage: &TokenUsage, fast: bool) -> Option<f64> {
        (!self.pricing.is_zero()).then(|| usage.estimate(&self.pricing, fast))
    }

    /// What a subsidised turn *would* have cost at [`list_cost`]
    /// (Self::list_cost) rates, kept as a reference next to the `$0` it
    /// actually billed. `None` for every model that is not subsidised, so
    /// callers do not show a redundant list-price figure next to a real bill.
    pub fn subsidised_list_cost(&self, usage: &TokenUsage, fast: bool) -> Option<f64> {
        self.pricing
            .subsidised_by
            .is_some()
            .then(|| self.list_cost(usage, fast))
            .flatten()
    }

    /// Name of the subscription covering this model's cost (e.g. `"Max"`),
    /// or `None` when the model is billed per-token as usual.
    pub fn subsidy_source(&self) -> Option<&str> {
        self.pricing.subsidised_by.as_deref()
    }

    pub fn provider_display_name(&self) -> &'static str {
        ManifestRegistry::for_slug(&self.provider).map_or("Unknown", |m| m.display_name)
    }

    pub fn from_tier(slug: &str, tier: ModelTier) -> Result<Self, ModelError> {
        if let Some(spec) = model_registry::spec_for_tier(slug, tier) {
            return Self::from_spec(&spec);
        }
        let entry = ManifestRegistry::find_default_for_tier(slug, tier)
            .ok_or_else(|| ModelError::NoDefault(slug.to_string(), tier))?;
        let model_id = entry.prefixes[0];
        Self::from_spec(&format!("{slug}/{model_id}"))
    }

    pub fn from_tier_with_policy(
        slug: &str,
        tier: ModelTier,
        policy: &ModelPolicy,
    ) -> Result<Self, ModelError> {
        if let Ok(model) = Self::from_tier_dynamic(slug, tier)
            && policy.allows(&model.spec())
        {
            return Ok(model);
        }

        let Some(manifest) = ManifestRegistry::for_slug(slug) else {
            return Err(ModelError::NoAllowedModel(slug.to_string(), tier));
        };
        manifest
            .models
            .iter()
            .filter(|entry| entry.tier == tier)
            .flat_map(|entry| entry.prefixes)
            .map(|model_id| format!("{slug}/{model_id}"))
            .find(|spec| policy.allows(spec))
            .map(|spec| Self::from_spec(&spec))
            .transpose()?
            .ok_or_else(|| ModelError::NoAllowedModel(slug.to_string(), tier))
    }

    pub fn from_tier_dynamic(slug: &str, tier: ModelTier) -> Result<Self, ModelError> {
        if let Some(model) = dynamic::find_model_for_tier(slug, tier) {
            return Ok(model);
        }
        // One providers.toml read, three answers: a model declared at this tier,
        // the provider exists but declares nothing here (inherit the base
        // protocol default under the custom slug, keeping its tier and pricing),
        // or no such provider.
        match custom::resolve_tier(slug, tier) {
            custom::TierLookup::Model(model) => return Ok(model),
            custom::TierLookup::NoModelForTier(base) => {
                let manifest = ManifestRegistry::get(&base.to_string())
                    .ok_or_else(|| ModelError::NoDefault(slug.to_string(), tier))?;
                let entry = manifest
                    .models
                    .iter()
                    .find(|e| e.default && e.tier == tier)
                    .ok_or_else(|| ModelError::NoDefault(slug.to_string(), tier))?;
                return Ok(Self::from_base(manifest, slug, entry.prefixes[0]));
            }
            custom::TierLookup::Unknown => {}
        }
        // Builtin or dynamic slug: resolve the base default under the slug
        // (dynamic slugs route through `base_for_slug`).
        if ManifestRegistry::get(slug).is_some() || dynamic::base_for_slug(slug).is_some() {
            return Self::from_tier(slug, tier);
        }
        Err(ModelError::UnsupportedProvider(slug.to_string()))
    }

    pub fn from_spec_with_policy(spec: &str, policy: &ModelPolicy) -> Result<Self, ModelError> {
        if !policy.allows(spec) {
            return Err(ModelError::NotAllowed(spec.to_string()));
        }
        Self::from_spec(spec)
    }

    pub fn from_spec(spec: &str) -> Result<Self, ModelError> {
        let (slug, model_id) = spec.split_once('/').ok_or(ModelError::InvalidFormat)?;

        // Precedence: builtin, then dynamic script, then providers.toml custom,
        // then models.dev catalogue sub-provider.
        // Discovery drops any script slug a builtin or custom entry already owns,
        // so a script and a custom provider can never share a slug here.
        if let Some(manifest) = ManifestRegistry::get(slug) {
            return Ok(Self::from_base(manifest, slug, model_id));
        }

        if let Some(model) = dynamic::lookup_model(slug, model_id) {
            return Ok(model);
        }

        if let Some(base) = dynamic::base_for_slug(slug)
            && let Some(manifest) = ManifestRegistry::get(&base.to_string())
        {
            return Ok(Self::from_base(manifest, slug, model_id));
        }

        if let Some(model) = custom::lookup_model(slug, model_id) {
            return Ok(model);
        }

        if let Some(meta) = crate::providers::catalog::model_meta_if_available(slug, model_id) {
            return Ok(Self::from_catalog(slug, model_id, meta));
        }

        Err(ModelError::UnsupportedProvider(slug.to_string()))
    }

    /// Free public models surfaced through the OpenCode provider (Zen/Go),
    /// using the catalog's definition of free (zero input and output price),
    /// the same one that gates `enable_free_models`, plus models a provider's
    /// `/models` call reported at an explicit zero price.
    ///
    /// Queries the live catalog rather than `self.pricing`, which may not yet
    /// reflect catalog prices when discovery hasn't seeded the registry, and
    /// which reads zero for "price unknown" too.
    pub fn is_free(&self) -> bool {
        self.discovered_free
            || crate::providers::catalog::free_model_if_available(&self.provider, &self.id)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Non-cached input tokens. Total input = `input + cache_read + cache_creation`.
    #[serde(rename = "input_tokens")]
    pub input: u32,
    #[serde(rename = "output_tokens")]
    pub output: u32,
    #[serde(rename = "cache_creation_input_tokens")]
    pub cache_creation: u32,
    #[serde(rename = "cache_read_input_tokens")]
    pub cache_read: u32,
    /// What this one response cost, straight from the provider, when it bothers
    /// to say (OpenRouter's `usage.cost`). Worth preferring on a router, where
    /// our table prices the model we asked for and the router bills for
    /// whichever upstream it picked.
    ///
    /// One response only, so it is neither summed nor stored. Sessions add up
    /// their bill a turn at a time in [`StoredTokenUsage::cost`].
    #[serde(skip)]
    pub cost: Option<f64>,
}

impl From<StoredTokenUsage> for TokenUsage {
    fn from(s: StoredTokenUsage) -> Self {
        Self {
            input: s.input,
            output: s.output,
            cache_creation: s.cache_creation,
            cache_read: s.cache_read,
            // A stored cost belongs to a whole session and this field to one
            // response, so there is nothing honest to carry across.
            cost: None,
        }
    }
}

impl TokenUsage {
    /// Ready to store, with what the turn was billed. No `From<TokenUsage>` on
    /// purpose: a caller that forgets the cost quietly loses money from the
    /// session total, so saying it out loud is mandatory.
    pub fn billed(&self, cost: Option<f64>) -> StoredTokenUsage {
        self.billed_with_list_cost(cost, None)
    }

    /// Like [`billed`](Self::billed), but also records what a subsidised
    /// turn would have cost at the provider's list rates. `list_cost` is
    /// `None` for every non-subsidised turn.
    pub fn billed_with_list_cost(
        &self,
        cost: Option<f64>,
        list_cost: Option<f64>,
    ) -> StoredTokenUsage {
        StoredTokenUsage {
            input: self.input,
            output: self.output,
            cache_creation: self.cache_creation,
            cache_read: self.cache_read,
            cost,
            list_cost,
        }
    }

    pub fn total_input(&self) -> u32 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_creation)
    }

    pub fn context_tokens(&self) -> u32 {
        self.total_input().saturating_add(self.output)
    }

    pub fn format(&self, cost: Option<f64>) -> String {
        self.format_cost(cost, "")
    }

    /// Like [`format`](Self::format), but marks the cost as a running total.
    pub fn format_sum_cost(&self, cost: Option<f64>) -> String {
        self.format_cost(cost, "Σ")
    }

    fn format_cost(&self, cost: Option<f64>, prefix: &str) -> String {
        let tokens = format!(
            "{}↑ {}↓",
            format_tokens(self.total_input()),
            format_tokens(self.output)
        );
        match cost {
            Some(cost) => format!("{tokens} {prefix}${cost:.3}"),
            None => tokens,
        }
    }

    /// Crate-private on purpose: pricing outside [`Model`] skips the provider's
    /// schedule, and the bill it may have sent us.
    pub(crate) fn estimate(&self, pricing: &ModelPricing, fast: bool) -> f64 {
        let (input, output, cache_write, cache_read) = pricing.rates(fast);
        self.input as f64 * input / PER_MILLION
            + self.output as f64 * output / PER_MILLION
            + self.cache_creation as f64 * cache_write / PER_MILLION
            + self.cache_read as f64 * cache_read / PER_MILLION
    }
}

pub fn format_tokens(tokens: impl Into<u64>) -> String {
    let tokens = tokens.into();
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => format!("{:.1}k", tokens as f64 / 1_000.0),
        _ => format!("{:.1}m", tokens as f64 / 1_000_000.0),
    }
}

impl AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input = self.input.saturating_add(rhs.input);
        self.output = self.output.saturating_add(rhs.output);
        self.cache_creation = self.cache_creation.saturating_add(rhs.cache_creation);
        self.cache_read = self.cache_read.saturating_add(rhs.cache_read);
        // Only some responses arrive with a bill, so a running total of them is
        // part paid and part missing while looking like the lot.
        self.cost = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn policy(allowed: &[&str], excluded: &[&str]) -> ModelPolicy {
        ModelPolicy::new(
            &allowed
                .iter()
                .map(|pattern| (*pattern).into())
                .collect::<Vec<_>>(),
            &excluded
                .iter()
                .map(|pattern| (*pattern).into())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    const TIERS: [ModelTier; 4] = [
        ModelTier::Weak,
        ModelTier::Medium,
        ModelTier::Strong,
        ModelTier::Compaction,
    ];

    const EPSILON: f64 = 1e-10;
    /// The only builtin whose rates move with the wall clock.
    const SCHEDULED_PROVIDERS: [&str; 1] = ["deepseek"];
    const DEEPSEEK_SPEC: &str = "deepseek/deepseek-v4-pro";
    const UNPRICED_DEEPSEEK_SPEC: &str = "deepseek/my-custom-model";
    const MILLION: u32 = 1_000_000;
    const INPUT_ONLY: TokenUsage = TokenUsage {
        input: MILLION,
        output: 0,
        cache_creation: 0,
        cache_read: 0,
        cost: None,
    };
    /// Four counters that cannot be confused with each other.
    const COUNTERS: TokenUsage = TokenUsage {
        input: 11,
        output: 22,
        cache_creation: 33,
        cache_read: 44,
        cost: None,
    };
    const RECORDED_COST: f64 = 0.25;
    const FREE_MEANS_A_KNOWN_ZERO: &str = "only a price discovery reported as zero means free";
    const TABLE_MUST_NOT_AGREE_BY_LUCK: &str =
        "the table has to disagree, or preferring the bill proves nothing";
    const PAID_PRICING: ModelPricing = ModelPricing::per_token(3.0, 15.0, 0.0, 0.0);

    #[test_case(999, "999"         ; "under_thousand")]
    #[test_case(1_000, "1.0k"      ; "thousand")]
    #[test_case(999_999, "1000.0k" ; "just_under_million")]
    #[test_case(1_000_000, "1.0m"  ; "million")]
    fn format_tokens_display(tokens: u32, expected: &str) {
        assert_eq!(format_tokens(tokens), expected);
    }

    #[test_case(TokenUsage { input: 12_000, output: 456, cache_creation: 200, cache_read: 100, cost: None }, None, "12.3k↑ 456↓" ; "without_cost")]
    #[test_case(TokenUsage { input: 1_000_000, output: 100_000, cache_creation: 200_000, cache_read: 500_000, cost: None }, Some(5.4), "1.7m↑ 100.0k↓ $5.400" ; "with_cost")]
    #[test_case(TokenUsage { input: u32::MAX, output: 1, cache_creation: 1, cache_read: 1, cost: None }, None, "4295.0m↑ 1↓" ; "input_saturates")]
    fn usage_formatting(usage: TokenUsage, cost: Option<f64>, expected: &str) {
        assert_eq!(usage.format(cost), expected);
    }

    #[test]
    fn sum_marker_applies_only_to_the_cost() {
        let usage = TokenUsage {
            input: 12_000,
            output: 456,
            cache_creation: 200,
            cache_read: 100,
            ..Default::default()
        };
        assert_eq!(usage.format_sum_cost(Some(1.5)), "12.3k↑ 456↓ Σ$1.500");
        assert_eq!(usage.format_sum_cost(None), usage.format(None));
    }

    #[test_case("no-slash-here", ModelError::InvalidFormat ; "invalid_format")]
    #[test_case("foobar/gpt-4", ModelError::UnsupportedProvider("foobar".into()) ; "unsupported_provider")]
    fn from_spec_errors(spec: &str, expected: ModelError) {
        let err = Model::from_spec(spec).unwrap_err();
        assert_eq!(
            std::mem::discriminant(&err),
            std::mem::discriminant(&expected)
        );
    }

    #[test]
    fn from_spec_with_policy_rejects_disallowed_exact_spec() {
        let policy = policy(&["anthropic/*"], &[]);
        let spec = "openai/gpt-5.6-sol";

        let error = Model::from_spec_with_policy(spec, &policy).unwrap_err();

        assert!(matches!(error, ModelError::NotAllowed(disallowed) if disallowed == spec));
    }

    #[test]
    fn from_spec_with_policy_resolves_allowed_exact_spec() {
        let policy = policy(&["openai/gpt-5.6-sol"], &[]);

        let model = Model::from_spec_with_policy("openai/gpt-5.6-sol", &policy).unwrap();

        assert_eq!(model.spec(), "openai/gpt-5.6-sol");
    }

    #[test]
    fn tier_with_policy_uses_allowed_alternative() {
        let policy = policy(&["openai/gpt-5.4-nano"], &[]);

        let model = Model::from_tier_with_policy("openai", ModelTier::Weak, &policy).unwrap();

        assert_eq!(model.spec(), "openai/gpt-5.4-nano");
        assert_eq!(model.tier, ModelTier::Weak);
    }

    #[test]
    fn tier_with_policy_errors_without_allowed_candidate() {
        let policy = policy(&["anthropic/*"], &[]);

        let error = Model::from_tier_with_policy("openai", ModelTier::Weak, &policy).unwrap_err();

        assert!(matches!(
            error,
            ModelError::NoAllowedModel(provider, ModelTier::Weak) if provider == "openai"
        ));
    }

    #[test]
    fn from_spec_unknown_catalogue_subprovider_is_unsupported() {
        // The on-disk models.dev cache may populate the catalog in a
        // developer's environment, so pick a slug that is likely not in any
        // catalog and confirm it falls through to the generic
        // unsupported-provider branch.
        let err = Model::from_spec("definitely-not-a-catalog-slug/any-model").unwrap_err();
        assert!(matches!(err, ModelError::UnsupportedProvider(_)));
    }

    #[test]
    fn total_input_includes_cached_tokens() {
        let usage = TokenUsage {
            input: 5_000,
            output: 1_000,
            cache_creation: 10_000,
            cache_read: 150_000,
            ..Default::default()
        };
        assert_eq!(usage.total_input(), 165_000);
    }

    #[test]
    fn estimate_computes_all_token_types() {
        let pricing = ModelPricing::per_token(3.00, 15.00, 3.75, 0.30);
        let usage = TokenUsage {
            input: 1_000_000,
            output: 100_000,
            cache_creation: 200_000,
            cache_read: 500_000,
            ..Default::default()
        };
        let cost = usage.estimate(&pricing, false);
        let expected = 3.0 + 1.5 + 0.75 + 0.15;
        assert!((cost - expected).abs() < 1e-10);
    }

    /// A bill wins outright, and the two ways it used to get lost are the two
    /// cases here: DeepSeek would have scaled it by the hour on top, and an
    /// unpriced model would have thrown it away for having no rate to quote.
    #[test_case(DEEPSEEK_SPEC ; "priced_and_scheduled")]
    #[test_case(UNPRICED_DEEPSEEK_SPEC ; "unpriced")]
    fn a_reported_cost_is_the_whole_answer(spec: &str) {
        let model = Model::from_spec(spec).unwrap();
        let billed = TokenUsage {
            cost: Some(RECORDED_COST),
            ..INPUT_ONLY
        };
        assert_ne!(
            model.billed_cost(&INPUT_ONLY, false),
            Some(RECORDED_COST),
            "{TABLE_MUST_NOT_AGREE_BY_LUCK}"
        );
        assert_eq!(model.billed_cost(&billed, false), Some(RECORDED_COST));
    }

    /// Only some responses arrive with a bill, so a total of them would be part
    /// paid and part missing while looking like the whole session.
    #[test]
    fn adding_usage_sums_counters_and_drops_the_bill() {
        let mut total = TokenUsage {
            cost: Some(RECORDED_COST),
            ..COUNTERS
        };
        total += TokenUsage {
            cost: Some(RECORDED_COST),
            ..COUNTERS
        };
        assert_eq!(total.input, COUNTERS.input * 2);
        assert_eq!(total.cost, None);
    }

    #[test]
    fn fast_mode_applies_premium_rates() {
        let pricing = ModelPricing::per_token_with_fast(5.00, 25.00, 6.25, 0.50, 30.00, 150.00);
        let usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_creation: 1_000_000,
            cache_read: 1_000_000,
            ..Default::default()
        };
        let fast = usage.estimate(&pricing, true);
        let expected = 30.0 + 150.0 + 37.5 + 3.0;
        assert!((fast - expected).abs() < 1e-10);
        assert!(fast > usage.estimate(&pricing, false));
    }

    #[test]
    fn fast_flag_ignored_without_fast_tier() {
        let pricing = ModelPricing::per_token(3.00, 15.00, 3.75, 0.30);
        let usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_creation: 0,
            cache_read: 0,
            ..Default::default()
        };
        assert_eq!(
            usage.estimate(&pricing, true),
            usage.estimate(&pricing, false)
        );
    }

    #[test]
    fn fast_pricing_is_always_a_premium() {
        for manifest in ManifestRegistry::builtins() {
            for entry in manifest.models {
                let Some(fast) = &entry.pricing.fast else {
                    continue;
                };
                assert!(
                    fast.input >= entry.pricing.input && fast.output >= entry.pricing.output,
                    "{}/{}: fast pricing must not be cheaper than standard",
                    manifest.slug,
                    entry.prefixes[0],
                );
            }
        }
    }

    #[test]
    fn spec_roundtrip() {
        for manifest in ManifestRegistry::builtins() {
            if manifest.accepts_arbitrary_models {
                continue;
            }
            let model = Model::from_tier(manifest.slug, ModelTier::Medium).unwrap();
            let round = Model::from_spec(&model.spec()).unwrap();
            assert_eq!(round.id, model.id);
            assert_eq!(round.provider, model.provider);
        }
    }

    #[test]
    fn opencode_from_spec_parses_four_levels() {
        let spec = "opencode/nvidia/openai/gpt-oss-120b";
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.provider, Arc::<str>::from("opencode"));
        assert_eq!(model.id, "nvidia/openai/gpt-oss-120b");
        assert_eq!(model.spec(), spec);
    }

    #[test]
    fn opencode_from_spec_parses_three_levels() {
        let spec = "opencode/opencode/big-pickle";
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.provider, Arc::<str>::from("opencode"));
        assert_eq!(model.id, "opencode/big-pickle");
        assert_eq!(model.spec(), spec);
    }

    #[test]
    fn from_tier_covers_all_providers() {
        for manifest in ManifestRegistry::builtins() {
            if manifest.accepts_arbitrary_models {
                continue;
            }
            let slug: Arc<str> = Arc::from(manifest.slug);
            for &tier in &TIERS {
                // DeepSeek has no Weak tier model
                if manifest.slug == "deepseek" && tier == ModelTier::Weak {
                    continue;
                }
                // Compaction is user-assigned only, not in static registry
                if tier == ModelTier::Compaction {
                    continue;
                }
                let model = Model::from_tier(manifest.slug, tier).unwrap();
                assert_eq!(model.provider, slug);
                assert_eq!(model.tier, tier);
                let max_output = model.max_output_tokens.unwrap();
                assert!(max_output > 0);
                assert!(model.context_window >= max_output);
            }
        }
    }

    #[test]
    fn tier_display_roundtrip() {
        for &tier in &TIERS {
            let s = tier.to_string();
            assert_eq!(s.parse::<ModelTier>().unwrap(), tier);
        }
        assert!(matches!(
            "turbo".parse::<ModelTier>(),
            Err(ModelError::InvalidTier(_))
        ));
    }

    #[test]
    fn exactly_one_default_per_provider_tier() {
        for manifest in ManifestRegistry::builtins() {
            if manifest.accepts_arbitrary_models {
                continue;
            }
            let entries = manifest.models;
            for &tier in &TIERS {
                if manifest.slug == "deepseek" && tier == ModelTier::Weak {
                    continue;
                }
                // Compaction is user-assigned only, not in static registry
                if tier == ModelTier::Compaction {
                    continue;
                }
                let count = entries
                    .iter()
                    .filter(|e| e.tier == tier && e.default)
                    .count();
                assert_eq!(
                    count, 1,
                    "{}/{}: expected exactly 1 default, found {count}",
                    manifest.slug, tier
                );
            }
        }
    }

    #[test_case("anthropic/claude-99-turbo", "anthropic", "claude-99-turbo" ; "unknown_anthropic_model_accepted")]
    #[test_case("zai/glm-99", "zai", "glm-99" ; "unknown_zai_model_accepted")]
    #[test_case("openai/gpt-99", "openai", "gpt-99" ; "unknown_openai_model_accepted")]
    #[test_case("xai/grok-99", "xai", "grok-99" ; "unknown_xai_model_accepted")]
    #[test_case("synthetic/hf:nonexistent", "synthetic", "hf:nonexistent" ; "unknown_synthetic_model_accepted")]
    #[test_case("ollama/my-custom-model", "ollama", "my-custom-model" ; "unknown_ollama_model_accepted")]
    #[test_case("deepseek/my-custom-model", "deepseek", "my-custom-model" ; "unknown_deepseek_model_accepted")]
    fn unknown_model_accepted(spec: &str, expected_slug: &str, expected_id: &str) {
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.provider, Arc::<str>::from(expected_slug));
        assert_eq!(model.id, expected_id);
        let manifest = ManifestRegistry::get(expected_slug).unwrap();
        assert_eq!(model.family, manifest.family);
    }

    #[test]
    fn from_base_unknown_model_uses_provider_fallbacks() {
        // Deliberately fake id so this stays valid when the model table changes.
        let model = Model::from_base(
            ManifestRegistry::get("anthropic").unwrap(),
            "anthropic",
            "claude-nonexistent-99",
        );
        assert_eq!(model.provider, Arc::<str>::from("anthropic"));
        assert_eq!(model.id, "claude-nonexistent-99");
        assert_eq!(model.spec(), "anthropic/claude-nonexistent-99");
        assert_eq!(model.family, ModelFamily::Claude);
        assert_eq!(model.max_output_tokens, Some(128_000));
        assert_eq!(model.context_window, 200_000);
        let p = &model.pricing;
        assert_eq!(
            (p.input, p.output, p.cache_write, p.cache_read),
            (0.0, 0.0, 0.0, 0.0)
        );
    }

    #[test_case("anthropic/claude-opus-4-8",       true  ; "claude")]
    #[test_case("openai/gpt-5.4",                   true  ; "gpt")]
    #[test_case("xai/grok-4.6",                     true  ; "grok")]
    #[test_case("google/gemini-2.5-pro",            true  ; "gemini")]
    #[test_case("copilot/claude-opus-4.7",          true  ; "copilot_entry_beats_generic_family")]
    #[test_case("zai/glm-5-code",                   false ; "glm_code_text_only")]
    #[test_case("deepseek/deepseek-v4-pro",         false ; "deepseek_text_only")]
    #[test_case("mistral/mistral-medium-latest",    true  ; "mistral_medium")]
    #[test_case("mistral/ministral-14b-latest",     false ; "ministral_text_only")]
    #[test_case("anthropic/claude-nonexistent-99",  true  ; "unknown_model_uses_family_fallback")]
    #[test_case("deepseek/my-custom-model",         false ; "unknown_generic_defaults_off")]
    fn vision_resolved_from_entry_or_family(spec: &str, expected: bool) {
        assert_eq!(Model::from_spec(spec).unwrap().supports_vision(), expected);
    }

    #[test_case("claude-opus-5",    true  ; "entry_with_fast_pricing")]
    #[test_case("claude-opus-5-1m", true  ; "long_context_suffix_still_matches_prefix")]
    #[test_case("claude-opus-4-7",  false ; "fast_withdrawn_from_the_table")]
    #[test_case("claude-sonnet-5",  false ; "entry_without_fast_pricing")]
    #[test_case("claude-opus-99",   false ; "no_entry_at_all")]
    fn supports_fast_follows_anthropic_table(model_id: &str, expected: bool) {
        let model = Model::from_base(
            ManifestRegistry::get("anthropic").unwrap(),
            "anthropic",
            model_id,
        );
        assert_eq!(model.supports_fast(), expected);
    }

    #[test]
    fn supports_fast_false_for_non_anthropic_even_with_fast_pricing() {
        let mut model = Model::from_base(
            ManifestRegistry::get("google").unwrap(),
            "google",
            "gemini-2.5-pro",
        );
        model.pricing.fast = Some(FastPricing {
            input: 30.0,
            output: 150.0,
        });
        assert!(!model.supports_fast());
    }

    #[test]
    fn discovered_vision_flows_into_curated_provider_model() {
        use crate::model::ModelInfo;

        model_registry::set_known_models(
            "synthetic",
            vec![
                ModelInfo {
                    supports_vision: Some(true),
                    ..ModelInfo::id_only("syn:test-vision".into())
                },
                ModelInfo::id_only("syn:test-blind".into()),
            ],
        );

        let vision = |id| Model::from_spec(id).unwrap().supports_vision();
        assert!(vision("synthetic/syn:test-vision"));
        assert!(!vision("synthetic/syn:test-blind"));
    }

    #[test]
    fn discovered_context_window_flows_into_from_base_for_unknown_model() {
        use crate::model::ModelInfo;

        let model_id = "test-discovered-context-window-model";
        let expected_window: u32 = 131_072;

        model_registry::set_known_models(
            "ollama",
            vec![ModelInfo {
                context_window: Some(expected_window),
                ..ModelInfo::id_only(model_id.to_string())
            }],
        );

        let model = Model::from_base(ManifestRegistry::get("ollama").unwrap(), "ollama", model_id);
        assert_eq!(model.context_window, expected_window);

        // A dynamic/custom slug shares its base provider's discovery.
        let wrapped = Model::from_base(
            ManifestRegistry::get("ollama").unwrap(),
            "my-ollama-wrap",
            model_id,
        );
        assert_eq!(wrapped.spec(), format!("my-ollama-wrap/{model_id}"));
        assert_eq!(wrapped.context_window, expected_window);
    }

    /// "We could not read a price" must never reach the picker as "free", so
    /// only an explicit zero from discovery sets the flag.
    #[test_case(Some(ModelPricing::ZERO), true  ; "explicit_zero_is_free")]
    #[test_case(Some(PAID_PRICING),       false ; "priced_is_not_free")]
    #[test_case(None,                     false ; "unknown_price_is_not_free")]
    fn discovered_pricing_decides_free(pricing: Option<ModelPricing>, expected: bool) {
        let model_id = "test-discovered-free-model";
        model_registry::set_known_models(
            "ollama",
            vec![ModelInfo {
                pricing,
                ..ModelInfo::id_only(model_id.to_string())
            }],
        );

        let model = Model::from_base(ManifestRegistry::get("ollama").unwrap(), "ollama", model_id);
        assert_eq!(model.is_free(), expected, "{FREE_MEANS_A_KNOWN_ZERO}");
    }

    /// A schedule hung on the wrong manifest silently doubles every turn of a
    /// provider that bills flat.
    #[test]
    fn only_deepseek_bills_by_the_clock() {
        let scheduled: Vec<&str> = ManifestRegistry::builtins()
            .iter()
            .filter(|m| m.pricing_schedule.is_some())
            .map(|m| m.slug)
            .collect();
        assert_eq!(scheduled, SCHEDULED_PROVIDERS);
    }

    /// Nothing else pins the wiring: a real DeepSeek model has to pick the
    /// schedule up out of its manifest, and `list_cost` has to stay out of it.
    /// `billed_cost` reads the real clock, so the expectation is sampled either
    /// side of the call in case the hour ticks over mid-test.
    #[test]
    fn deepseek_bills_its_peak_surcharge_on_top_of_the_table() {
        let model = Model::from_spec(DEEPSEEK_SPEC).unwrap();
        let schedule = ManifestRegistry::for_slug(&model.provider)
            .and_then(|m| m.pricing_schedule)
            .expect("deepseek bills by the clock");

        let list = model.list_cost(&INPUT_ONLY, false).unwrap();
        let table_price = f64::from(INPUT_ONLY.input) * model.pricing.input / PER_MILLION;
        assert!(
            (list - table_price).abs() < EPSILON,
            "list_cost {list} must be the table price {table_price}, surcharge free"
        );

        let before = schedule.multiplier_at(Timestamp::now());
        let billed = model.billed_cost(&INPUT_ONLY, false).unwrap();
        let after = schedule.multiplier_at(Timestamp::now());
        assert!(
            [before, after]
                .iter()
                .any(|multiplier| (billed - list * multiplier).abs() < EPSILON),
            "billed {billed} is not {list} scaled by the schedule ({before} or {after})"
        );
    }

    /// A schedule must not turn "no price" into "$0.000". Callers hide `None`,
    /// and any multiple of nothing is still nothing. A free model billing us
    /// zero lands in the same place, which is where it has always been.
    #[test]
    fn unpriced_models_stay_unpriced_under_a_schedule() {
        let model = Model::from_spec(UNPRICED_DEEPSEEK_SPEC).unwrap();
        let free = TokenUsage {
            cost: Some(0.0),
            ..INPUT_ONLY
        };
        assert!(model.pricing.is_zero());
        assert_eq!(model.list_cost(&INPUT_ONLY, false), None);
        assert_eq!(model.billed_cost(&INPUT_ONLY, false), None);
        assert_eq!(model.billed_cost(&free, false), None);
    }

    /// Every later total is rebuilt from what was stored, so storing a turn must
    /// not shuffle the counters, invent one, or drop the cost.
    #[test]
    fn billed_stores_every_counter_and_the_cost() {
        assert_eq!(
            COUNTERS.billed(Some(RECORDED_COST)),
            StoredTokenUsage {
                input: COUNTERS.input,
                output: COUNTERS.output,
                cache_creation: COUNTERS.cache_creation,
                cache_read: COUNTERS.cache_read,
                cost: Some(RECORDED_COST),
                list_cost: None,
            }
        );
        assert_eq!(COUNTERS.billed(None).cost, None);
    }

    /// A subsidy zeroes the bill while the list price stays visible as a
    /// reference, so "covered by the subscription" and "free" read apart.
    #[test]
    fn subsidised_pricing_bills_zero_and_keeps_the_list_price() {
        let mut model = Model::from_spec(UNPRICED_DEEPSEEK_SPEC).unwrap();
        model.pricing = ModelPricing {
            subsidised_by: Some(Arc::from("Max")),
            ..PAID_PRICING
        };
        let list = model.list_cost(&INPUT_ONLY, false).unwrap();
        assert!(list > 0.0);
        assert_eq!(model.billed_cost(&INPUT_ONLY, false), Some(0.0));
        assert_eq!(model.subsidised_list_cost(&INPUT_ONLY, false), Some(list));
        assert_eq!(model.subsidy_source(), Some("Max"));
    }

    /// A subsidy must not turn "no price table" into a false `$0.000`: with
    /// zero rates there is nothing to reference, so both costs stay `None`.
    #[test]
    fn subsidised_but_unpriced_model_stays_unpriced() {
        let mut model = Model::from_spec(UNPRICED_DEEPSEEK_SPEC).unwrap();
        assert!(model.pricing.is_zero());
        model.pricing.subsidised_by = Some(Arc::from("Max"));
        assert_eq!(model.billed_cost(&INPUT_ONLY, false), None);
        assert_eq!(model.subsidised_list_cost(&INPUT_ONLY, false), None);
    }

    /// An ordinary metered model has no list-price reference to show; the
    /// figure only appears next to a subsidised `$0` bill.
    #[test]
    fn metered_model_has_no_subsidised_list_cost() {
        let model = Model::from_spec(DEEPSEEK_SPEC).unwrap();
        assert!(model.billed_cost(&INPUT_ONLY, false).is_some());
        assert_eq!(model.subsidised_list_cost(&INPUT_ONLY, false), None);
        assert_eq!(model.subsidy_source(), None);
    }
}
