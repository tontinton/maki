use std::sync::Mutex;

use color_eyre::Result;
use color_eyre::eyre::Context;

use maki_providers::model::{Model, ModelTier};
use maki_providers::provider::ProviderKind;
use maki_storage::DataDir;
use maki_storage::log::RotatingFileWriter;
use maki_storage::model::{persist_model, read_model};
use tracing_subscriber::EnvFilter;

const PROVIDER_PRIORITY: &[ProviderKind] = &[
    ProviderKind::Anthropic,
    ProviderKind::Bedrock,
    ProviderKind::OpenAi,
    ProviderKind::Zai,
    ProviderKind::ZaiCodingPlan,
    ProviderKind::Synthetic,
];

pub fn resolve_model(
    explicit: Option<&str>,
    provider_config: &maki_config::ProviderConfig,
    storage: &DataDir,
) -> Result<Model> {
    if let Some(spec) = explicit {
        let model = Model::from_spec(spec).context("invalid --model spec")?;
        persist_model(storage, &model.spec());
        return Ok(model);
    }
    if let Some(spec) = read_model(storage) {
        if let Ok(m) = Model::from_spec(&spec) {
            return Ok(m);
        }
        tracing::warn!(spec, "saved model no longer valid, falling back to default");
    }
    if let Some(spec) = provider_config.default_model.as_deref() {
        return Model::from_spec(spec).context("invalid default_model in config");
    }
    auto_detect_model().ok_or_else(|| {
        color_eyre::eyre::eyre!(
            "no provider available - set an API key (e.g. ANTHROPIC_API_KEY) or run `maki auth login`\n\nSee https://maki.sh/docs/providers/ for setup instructions"
        )
    })
}

fn auto_detect_model() -> Option<Model> {
    for tier in [ModelTier::Strong, ModelTier::Medium] {
        for &provider in PROVIDER_PRIORITY {
            if provider.is_available()
                && let Ok(model) = Model::from_tier(provider, tier)
            {
                return Some(model);
            }
        }
    }
    None
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

pub fn init_logging(storage: &DataDir, storage_config: &maki_config::StorageConfig) {
    let Ok(writer) = RotatingFileWriter::new(
        storage,
        storage_config.max_log_bytes,
        storage_config.max_log_files,
    ) else {
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
