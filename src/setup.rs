use std::sync::Mutex;

use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};

use maki_providers::manifest::ManifestRegistry;
use maki_providers::model::{Model, ModelError, ModelTier};
use maki_storage::StateDir;
use maki_storage::log::RotatingFileWriter;
use maki_storage::model::read_model;
use tracing_subscriber::EnvFilter;

const PROVIDER_PRIORITY: &[&str] = &[
    "anthropic",
    "openai",
    "xai",
    "copilot",
    "zai",
    "synthetic",
    "deepseek",
];

pub fn resolve_model(
    explicit: Option<&str>,
    provider_config: &maki_config::ProviderConfig,
    storage: &StateDir,
) -> Result<Model> {
    let policy = &provider_config.model_policy;
    if let Some(spec) = explicit {
        if !policy.allows(spec) {
            return Err(eyre!(
                "model {spec:?} is not allowed by provider model policy"
            ));
        }
        return from_spec_or_warm_catalog(spec).context("invalid --model spec");
    }
    if let Some(spec) = read_model(storage) {
        if policy.allows(&spec)
            && let Ok(m) = from_spec_or_warm_catalog(&spec)
        {
            return Ok(m);
        }
        tracing::warn!(
            spec,
            "saved model unavailable or disallowed, falling back to default"
        );
    }
    if let Some(spec) = provider_config.default_model.as_deref() {
        if !policy.allows(spec) {
            return Err(eyre!(
                "default model {spec:?} is not allowed by provider model policy"
            ));
        }
        return from_spec_or_warm_catalog(spec).context("invalid default_model in config");
    }
    auto_detect_model(policy).ok_or_else(|| {
        let policy_note = if policy.is_restrictive() {
            "\nnote: an allowed_models/excluded_models policy is active and may exclude every candidate"
        } else {
            ""
        };
        color_eyre::eyre::eyre!(
            "no provider available - set an API key (e.g. ANTHROPIC_API_KEY), run `maki auth login`, or use -m to specify a model{policy_note}\n\nSee https://maki.sh/docs/providers/ for setup instructions"
        )
    })
}

/// An unknown slug may just mean the models.dev catalog has not been loaded
/// yet, so retry once with a warm catalog. `Model::from_spec` itself must stay
/// non-blocking: the UI draws with it.
fn from_spec_or_warm_catalog(spec: &str) -> Result<Model, ModelError> {
    match Model::from_spec(spec) {
        Err(ModelError::UnsupportedProvider(_)) => {
            maki_providers::warm_catalog();
            Model::from_spec(spec)
        }
        result => result,
    }
}

fn auto_detect_model(policy: &maki_config::ModelPolicy) -> Option<Model> {
    for tier in [ModelTier::Strong, ModelTier::Medium] {
        for &slug in PROVIDER_PRIORITY {
            if maki_providers::provider::provider_available(slug)
                && let Ok(model) = Model::from_tier(slug, tier)
                && policy.allows(&model.spec())
            {
                return Some(model);
            }
        }
    }
    None
}

/// Built-in slugs keep their compiled protocol, model catalog and auth wiring,
/// so a `providers.toml` entry setting those fields is only partly honored
/// (#597). Call this after `init_logging`, otherwise the warning has no
/// subscriber to reach.
pub fn warn_ignored_provider_fields() {
    for (slug, def) in &maki_config::providers::ProvidersConfig::load().providers {
        if ManifestRegistry::get(slug).is_none() {
            continue;
        }
        let ignored = maki_config::providers::ignored_builtin_fields(slug, def);
        if ignored.is_empty() {
            continue;
        }
        tracing::warn!(
            slug,
            fields = %ignored.join(", "),
            "providers.toml entry for built-in provider ignores these fields \
             (base_url/plan/api_key still apply), use a custom slug to set \
             protocol or models"
        );
    }
}

pub fn install_panic_log_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic payload".into()
        };
        let location = info.location().map(|l| l.to_string());
        tracing::error!(
            panic.payload = %payload,
            panic.location = location.as_deref().unwrap_or("<unknown>"),
            "panic occurred"
        );
        prev(info);
    }));
}

pub fn init_logging(storage_config: &maki_config::StorageConfig) {
    let Ok(writer) =
        RotatingFileWriter::new(storage_config.max_log_bytes, storage_config.max_log_files)
    else {
        return;
    };
    let writer = Mutex::new(writer);
    let filter = EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(writer)
        .init();
}
