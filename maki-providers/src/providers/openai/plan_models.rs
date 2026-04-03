use std::collections::{HashMap, hash_map::DefaultHasher};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use maki_storage::DataDir;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::AgentError;
use crate::model::{ModelEntry, ModelFamily, ModelPricing, ModelSet, ModelTier};
use crate::provider::ProviderKind;

use super::plan_codex_cli_version;

const CACHE_DIR: &str = "cache";
const CACHE_FILE_SUFFIX: &str = "models";
const DEFAULT_CACHE_SCOPE: &str = ProviderKind::OpenAiCodingPlan.base_url();

const STATIC_MODELS: &[ModelEntry] = &[
    ModelEntry {
        prefixes: &["gpt-5.4-mini"],
        tier: ModelTier::Weak,
        family: ModelFamily::Gpt,
        default: true,
        pricing: ModelPricing {
            input: 0.00,
            output: 0.00,
            cache_write: 0.00,
            cache_read: 0.00,
        },
        max_output_tokens: 128_000,
        context_window: 272_000,
    },
    ModelEntry {
        prefixes: &["gpt-5.1-codex-mini"],
        tier: ModelTier::Weak,
        family: ModelFamily::Gpt,
        default: false,
        pricing: ModelPricing {
            input: 0.00,
            output: 0.00,
            cache_write: 0.00,
            cache_read: 0.00,
        },
        max_output_tokens: 128_000,
        context_window: 272_000,
    },
    ModelEntry {
        prefixes: &["gpt-5.2"],
        tier: ModelTier::Medium,
        family: ModelFamily::Gpt,
        default: true,
        pricing: ModelPricing {
            input: 0.00,
            output: 0.00,
            cache_write: 0.00,
            cache_read: 0.00,
        },
        max_output_tokens: 128_000,
        context_window: 272_000,
    },
    ModelEntry {
        prefixes: &["gpt-5.4"],
        tier: ModelTier::Strong,
        family: ModelFamily::Gpt,
        default: true,
        pricing: ModelPricing {
            input: 0.00,
            output: 0.00,
            cache_write: 0.00,
            cache_read: 0.00,
        },
        max_output_tokens: 128_000,
        context_window: 272_000,
    },
    ModelEntry {
        prefixes: &["gpt-5.3-codex"],
        tier: ModelTier::Strong,
        family: ModelFamily::Gpt,
        default: false,
        pricing: ModelPricing {
            input: 0.00,
            output: 0.00,
            cache_write: 0.00,
            cache_read: 0.00,
        },
        max_output_tokens: 128_000,
        context_window: 272_000,
    },
    ModelEntry {
        prefixes: &["gpt-5.2-codex"],
        tier: ModelTier::Strong,
        family: ModelFamily::Gpt,
        default: false,
        pricing: ModelPricing {
            input: 0.00,
            output: 0.00,
            cache_write: 0.00,
            cache_read: 0.00,
        },
        max_output_tokens: 128_000,
        context_window: 272_000,
    },
    ModelEntry {
        prefixes: &["gpt-5.1-codex-max"],
        tier: ModelTier::Strong,
        family: ModelFamily::Gpt,
        default: false,
        pricing: ModelPricing {
            input: 0.00,
            output: 0.00,
            cache_write: 0.00,
            cache_read: 0.00,
        },
        max_output_tokens: 128_000,
        context_window: 272_000,
    },
];

#[derive(Deserialize, Serialize)]
struct ModelsResponse {
    models: Vec<ModelInfo>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct ModelInfo {
    slug: String,
    priority: i32,
    #[serde(default)]
    context_window: Option<u32>,
    #[serde(default)]
    visibility: Option<ModelVisibility>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ModelVisibility {
    List,
    Hide,
    None,
}

#[derive(Default)]
struct CachedModelsState {
    loaded: bool,
    models: Vec<ModelInfo>,
}

#[derive(Clone)]
pub(crate) struct PlanModelMetadata {
    pub tier: ModelTier,
    pub family: ModelFamily,
    pub pricing: ModelPricing,
    pub max_output_tokens: u32,
    pub context_window: u32,
}

impl PlanModelMetadata {
    fn from_entry(entry: &ModelEntry) -> Self {
        Self {
            tier: entry.tier,
            family: entry.family,
            pricing: entry.pricing.clone(),
            max_output_tokens: entry.max_output_tokens,
            context_window: entry.context_window,
        }
    }
}

pub(crate) fn models(set: ModelSet) -> &'static [ModelEntry] {
    match set {
        ModelSet::All | ModelSet::Visible => STATIC_MODELS,
    }
}

pub(crate) fn list_remote_models(cache_scope: &str, body: &str) -> Result<Vec<String>, AgentError> {
    let mut models = serde_json::from_str::<ModelsResponse>(body)?.models;
    sort_models(&mut models);
    cache_models(cache_scope, &models);
    persist_models(cache_scope, &models);
    Ok(models
        .into_iter()
        .filter(shows_in_picker)
        .map(|model| model.slug)
        .collect())
}

pub(crate) fn catalog_model_metadata(model_id: &str) -> Option<PlanModelMetadata> {
    STATIC_MODELS
        .iter()
        .find(|entry| entry.prefixes.contains(&model_id))
        .map(PlanModelMetadata::from_entry)
        .or_else(|| {
            cached_model_info(DEFAULT_CACHE_SCOPE, model_id)
                .map(|model| metadata_from_model(&model))
        })
        .or_else(|| {
            STATIC_MODELS
                .iter()
                .find(|entry| {
                    entry
                        .prefixes
                        .iter()
                        .any(|prefix| model_id.starts_with(prefix))
                })
                .map(PlanModelMetadata::from_entry)
        })
}

pub(crate) fn dynamic_model_metadata(model_id: &str) -> Option<PlanModelMetadata> {
    STATIC_MODELS
        .iter()
        .find(|entry| entry.prefixes.contains(&model_id))
        .map(PlanModelMetadata::from_entry)
        .or_else(|| {
            cached_model_info(DEFAULT_CACHE_SCOPE, model_id)
                .map(|model| metadata_from_model(&model))
        })
        .or_else(|| synthesized_metadata_for_slug(model_id))
        .or_else(|| {
            STATIC_MODELS
                .iter()
                .find(|entry| {
                    entry
                        .prefixes
                        .iter()
                        .any(|prefix| model_id.starts_with(prefix))
                })
                .map(PlanModelMetadata::from_entry)
        })
}

pub(crate) fn fallback_model_ids() -> Vec<String> {
    fallback_model_ids_for_scope(DEFAULT_CACHE_SCOPE)
}

fn fallback_model_ids_for_scope(cache_scope: &str) -> Vec<String> {
    let cached = cached_models_snapshot(cache_scope);
    if cached.is_empty() {
        return STATIC_MODELS
            .iter()
            .flat_map(|entry| entry.prefixes.iter())
            .map(|prefix| (*prefix).to_string())
            .collect();
    }

    let mut models = cached;
    sort_models(&mut models);
    models
        .into_iter()
        .filter(shows_in_picker)
        .map(|model| model.slug)
        .collect()
}

fn shows_in_picker(model: &ModelInfo) -> bool {
    model.visibility.unwrap_or(ModelVisibility::List) == ModelVisibility::List
}

fn sort_models(models: &mut [ModelInfo]) {
    models.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| b.slug.cmp(&a.slug))
    });
}

fn inferred_tier(slug: &str) -> ModelTier {
    if slug.contains("nano") || slug.contains("mini") {
        ModelTier::Weak
    } else if slug.contains("max") || slug.starts_with("gpt-5.") || slug.starts_with("gpt-5") {
        ModelTier::Strong
    } else {
        ModelTier::Medium
    }
}

fn inferred_max_output_tokens(slug: &str) -> u32 {
    if slug.starts_with("gpt-5") {
        128_000
    } else {
        100_000
    }
}

fn zero_pricing() -> ModelPricing {
    ModelPricing {
        input: 0.00,
        output: 0.00,
        cache_write: 0.00,
        cache_read: 0.00,
    }
}

fn metadata_from_model(model: &ModelInfo) -> PlanModelMetadata {
    PlanModelMetadata {
        tier: inferred_tier(&model.slug),
        family: ModelFamily::Gpt,
        pricing: zero_pricing(),
        max_output_tokens: inferred_max_output_tokens(&model.slug),
        context_window: model.context_window.unwrap_or(272_000),
    }
}

fn cached_models() -> &'static Mutex<HashMap<String, CachedModelsState>> {
    static CACHED: OnceLock<Mutex<HashMap<String, CachedModelsState>>> = OnceLock::new();
    CACHED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn current_version_tag() -> Option<String> {
    plan_codex_cli_version()
        .ok()
        .map(|version| sanitize_version_tag(&version))
}

fn sanitize_version_tag(version: &str) -> String {
    version
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => ch,
            _ => '-',
        })
        .collect()
}

fn cache_scope_key(cache_scope: &str) -> String {
    let scope = cache_scope.trim_end_matches('/');
    let version = current_version_tag().unwrap_or_else(|| "uninitialized".into());
    format!("{version}:{scope}")
}

fn default_cache_file_name() -> Option<String> {
    let version = current_version_tag()?;
    Some(format!(
        "{}-{CACHE_FILE_SUFFIX}-{version}.json",
        ProviderKind::OpenAiCodingPlan.slug()
    ))
}

fn scoped_cache_file_name(cache_scope: &str) -> Option<String> {
    let version = current_version_tag()?;
    let mut hasher = DefaultHasher::new();
    cache_scope.trim_end_matches('/').hash(&mut hasher);
    Some(format!(
        "{}-{CACHE_FILE_SUFFIX}-{version}-{:016x}.json",
        ProviderKind::OpenAiCodingPlan.slug(),
        hasher.finish()
    ))
}

fn cache_path(cache_scope: &str) -> Result<PathBuf, AgentError> {
    #[cfg(test)]
    if let Some(path) = test_cache_path_override(cache_scope) {
        return Ok(path);
    }

    let file_name = if cache_scope.trim_end_matches('/') == DEFAULT_CACHE_SCOPE {
        default_cache_file_name()
    } else {
        scoped_cache_file_name(cache_scope)
    };
    let Some(file_name) = file_name else {
        return Err(AgentError::Config {
            message: "OpenAI Coding Plan cache version not initialized".into(),
        });
    };

    let dir = DataDir::resolve()?;
    Ok(dir.ensure_subdir(CACHE_DIR)?.join(file_name))
}

fn read_cached_models_from_path(path: &Path) -> Result<Vec<ModelInfo>, AgentError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let body = fs::read_to_string(path)?;
    Ok(serde_json::from_str::<ModelsResponse>(&body)?.models)
}

fn write_models_to_path(path: &Path, models: &[ModelInfo], error_message: &'static str) {
    if let Some(parent) = path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        warn!(path = %path.display(), error = %error, "{error_message}");
        return;
    }
    let payload = match serde_json::to_vec_pretty(&ModelsResponse {
        models: models.to_vec(),
    }) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(path = %path.display(), error = %error, "{error_message}");
            return;
        }
    };
    if let Err(error) = fs::write(path, payload) {
        warn!(path = %path.display(), error = %error, "{error_message}");
    }
}

fn load_persisted_models(cache_scope: &str) -> Vec<ModelInfo> {
    let Ok(path) = cache_path(cache_scope) else {
        return Vec::new();
    };
    match read_cached_models_from_path(&path) {
        Ok(models) => models,
        Err(error) => {
            warn!(
                path = %path.display(),
                error = %error,
                "failed to load cached OpenAI Coding Plan model snapshot"
            );
            Vec::new()
        }
    }
}

fn persist_models(cache_scope: &str, models: &[ModelInfo]) {
    let Ok(path) = cache_path(cache_scope) else {
        return;
    };
    write_models_to_path(
        &path,
        models,
        "failed to persist OpenAI Coding Plan model snapshot",
    );
}

fn cached_models_snapshot(cache_scope: &str) -> Vec<ModelInfo> {
    let key = cache_scope_key(cache_scope);
    let mut state = cached_models().lock().unwrap();
    let entry = state.entry(key).or_default();
    if !entry.loaded {
        entry.models = load_persisted_models(cache_scope);
        entry.loaded = true;
    }
    entry.models.clone()
}

fn cache_models(cache_scope: &str, models: &[ModelInfo]) {
    let key = cache_scope_key(cache_scope);
    let mut state = cached_models().lock().unwrap();
    let entry = state.entry(key).or_default();
    entry.loaded = true;
    entry.models = models.to_vec();
}

fn cached_model_info(cache_scope: &str, model_id: &str) -> Option<ModelInfo> {
    cached_models_snapshot(cache_scope)
        .iter()
        .find(|model| model.slug == model_id)
        .cloned()
}

fn synthesized_metadata_for_slug(model_id: &str) -> Option<PlanModelMetadata> {
    if !looks_like_openai_plan_model(model_id) {
        return None;
    }

    Some(metadata_from_model(&ModelInfo {
        slug: model_id.to_string(),
        priority: 0,
        context_window: None,
        visibility: Some(ModelVisibility::List),
    }))
}

fn looks_like_openai_plan_model(model_id: &str) -> bool {
    model_id
        .strip_prefix("gpt-")
        .and_then(|suffix| suffix.chars().next())
        .is_some_and(|ch| ch.is_ascii_digit())
        || model_id
            .strip_prefix('o')
            .and_then(|suffix| suffix.chars().next())
            .is_some_and(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
fn test_cache_path_overrides() -> &'static Mutex<HashMap<String, PathBuf>> {
    static OVERRIDE: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    OVERRIDE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn test_cache_path_override(cache_scope: &str) -> Option<PathBuf> {
    test_cache_path_overrides()
        .lock()
        .unwrap()
        .get(&cache_scope_key(cache_scope))
        .cloned()
}

#[cfg(test)]
fn reset_cached_models_for_tests() {
    cached_models().lock().unwrap().clear();
}

#[cfg(test)]
pub(crate) fn cache_default_models_for_tests(body: &str) {
    let models = serde_json::from_str::<ModelsResponse>(body)
        .expect("test cached models json must be valid")
        .models;
    cache_models(DEFAULT_CACHE_SCOPE, &models);
}

#[cfg(test)]
pub(crate) fn reset_plan_models_for_tests() {
    reset_cached_models_for_tests();
}

#[cfg(test)]
mod tests {
    use super::super::{plan_test_lock, reset_plan_codex_cli_version};
    use super::*;
    use crate::set_openai_plan_codex_cli_version;
    use tempfile::TempDir;

    const DEFAULT_SCOPE: &str = DEFAULT_CACHE_SCOPE;
    const CUSTOM_SCOPE: &str = "https://proxy.example/codex";
    const TEST_CODEX_CLI_VERSION: &str = "0.118.0";

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        plan_test_lock()
    }

    fn with_cache_path(cache_scope: &str, path: PathBuf) {
        test_cache_path_overrides()
            .lock()
            .unwrap()
            .insert(cache_scope_key(cache_scope), path);
        reset_cached_models_for_tests();
    }

    fn clear_cache_path(cache_scope: &str) {
        test_cache_path_overrides()
            .lock()
            .unwrap()
            .remove(&cache_scope_key(cache_scope));
        reset_cached_models_for_tests();
    }

    fn write_cache(cache_scope: &str, models: &[ModelInfo]) {
        let path = cache_path(cache_scope).unwrap();
        write_models_to_path(&path, models, "failed to write test cache");
        reset_cached_models_for_tests();
    }

    fn init_version() {
        reset_plan_codex_cli_version();
        set_openai_plan_codex_cli_version(TEST_CODEX_CLI_VERSION);
    }

    #[test]
    fn parse_models_response_sorts_by_priority_then_slug_desc() {
        let _lock = test_lock();
        init_version();
        let body = r#"{
            "models": [
                {"slug": "gpt-5.3-codex", "priority": 0},
                {"slug": "gpt-5.4", "priority": 0},
                {"slug": "gpt-5.2-codex", "priority": 10}
            ]
        }"#;

        let mut models = serde_json::from_str::<ModelsResponse>(body).unwrap().models;
        sort_models(&mut models);
        let slugs: Vec<String> = models.into_iter().map(|model| model.slug).collect();

        assert_eq!(slugs, vec!["gpt-5.4", "gpt-5.3-codex", "gpt-5.2-codex"]);
    }

    #[test]
    fn default_cache_path_uses_single_versioned_file() {
        let _lock = test_lock();
        init_version();
        let name = default_cache_file_name().unwrap();

        assert_eq!(name, "openai-coding-plan-models-0.118.0.json");
    }

    #[test]
    fn custom_cache_path_is_scoped_by_endpoint_and_version() {
        let _lock = test_lock();
        init_version();
        let name = scoped_cache_file_name(CUSTOM_SCOPE).unwrap();

        assert!(name.starts_with("openai-coding-plan-models-0.118.0-"));
        assert!(name.ends_with(".json"));
    }

    #[test]
    fn first_run_without_cached_snapshot_uses_static_catalog_only() {
        let _lock = test_lock();
        init_version();
        let tmp = TempDir::new().unwrap();
        with_cache_path(DEFAULT_SCOPE, tmp.path().join("missing.json"));

        assert!(catalog_model_metadata("gpt-5.4").is_some());
        assert!(catalog_model_metadata("gpt-5.3-codex").is_some());
        assert_eq!(
            fallback_model_ids_for_scope(DEFAULT_SCOPE),
            vec![
                "gpt-5.4-mini",
                "gpt-5.1-codex-mini",
                "gpt-5.2",
                "gpt-5.4",
                "gpt-5.3-codex",
                "gpt-5.2-codex",
                "gpt-5.1-codex-max",
            ]
        );

        clear_cache_path(DEFAULT_SCOPE);
    }

    #[test]
    fn catalog_model_metadata_uses_cached_snapshot() {
        let _lock = test_lock();
        init_version();
        let tmp = TempDir::new().unwrap();
        with_cache_path(DEFAULT_SCOPE, tmp.path().join("snapshot.json"));
        write_cache(
            DEFAULT_SCOPE,
            &[ModelInfo {
                slug: "gpt-5.3-codex".into(),
                priority: 2,
                context_window: Some(272_000),
                visibility: Some(ModelVisibility::List),
            }],
        );

        let model = catalog_model_metadata("gpt-5.3-codex").unwrap();
        assert_eq!(model.tier, ModelTier::Strong);
        assert_eq!(model.max_output_tokens, 128_000);
        assert_eq!(model.context_window, 272_000);

        clear_cache_path(DEFAULT_SCOPE);
    }

    #[test]
    fn list_remote_models_persists_latest_results_without_merging() {
        let _lock = test_lock();
        init_version();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("snapshot.json");
        with_cache_path(DEFAULT_SCOPE, path.clone());
        write_cache(
            DEFAULT_SCOPE,
            &[
                ModelInfo {
                    slug: "gpt-5.4".into(),
                    priority: 0,
                    context_window: Some(1_050_000),
                    visibility: Some(ModelVisibility::List),
                },
                ModelInfo {
                    slug: "gpt-5.3-codex".into(),
                    priority: 2,
                    context_window: Some(272_000),
                    visibility: Some(ModelVisibility::List),
                },
            ],
        );

        let listed = list_remote_models(
            DEFAULT_SCOPE,
            r#"{
                "models": [
                    {"slug": "gpt-5.4", "priority": 0, "context_window": 1050000, "visibility": "list"}
                ]
            }"#,
        )
        .unwrap();
        let persisted = read_cached_models_from_path(&path).unwrap();

        assert_eq!(listed, vec!["gpt-5.4"]);
        assert_eq!(
            persisted,
            vec![ModelInfo {
                slug: "gpt-5.4".into(),
                priority: 0,
                context_window: Some(1_050_000),
                visibility: Some(ModelVisibility::List),
            }]
        );

        clear_cache_path(DEFAULT_SCOPE);
    }

    #[test]
    fn fallback_model_ids_use_cached_visible_models_only() {
        let _lock = test_lock();
        init_version();
        let tmp = TempDir::new().unwrap();
        with_cache_path(DEFAULT_SCOPE, tmp.path().join("snapshot.json"));
        write_cache(
            DEFAULT_SCOPE,
            &[
                ModelInfo {
                    slug: "gpt-5.5-codex".into(),
                    priority: 0,
                    context_window: Some(512_000),
                    visibility: Some(ModelVisibility::List),
                },
                ModelInfo {
                    slug: "gpt-5".into(),
                    priority: 10,
                    context_window: None,
                    visibility: Some(ModelVisibility::Hide),
                },
            ],
        );

        let models = fallback_model_ids_for_scope(DEFAULT_SCOPE);
        assert_eq!(models, vec!["gpt-5.5-codex"]);

        clear_cache_path(DEFAULT_SCOPE);
    }

    #[test]
    fn cached_model_snapshot_is_scoped_by_endpoint_and_version() {
        let _lock = test_lock();
        init_version();
        let tmp = TempDir::new().unwrap();
        with_cache_path(DEFAULT_SCOPE, tmp.path().join("default.json"));
        with_cache_path(CUSTOM_SCOPE, tmp.path().join("custom.json"));
        write_cache(
            DEFAULT_SCOPE,
            &[ModelInfo {
                slug: "gpt-5.5-codex".into(),
                priority: 0,
                context_window: Some(512_000),
                visibility: Some(ModelVisibility::List),
            }],
        );
        write_cache(
            CUSTOM_SCOPE,
            &[ModelInfo {
                slug: "gpt-6-proxy".into(),
                priority: 0,
                context_window: Some(256_000),
                visibility: Some(ModelVisibility::List),
            }],
        );

        assert_eq!(
            fallback_model_ids_for_scope(DEFAULT_SCOPE),
            vec!["gpt-5.5-codex"]
        );
        assert_eq!(
            fallback_model_ids_for_scope(CUSTOM_SCOPE),
            vec!["gpt-6-proxy"]
        );

        clear_cache_path(DEFAULT_SCOPE);
        clear_cache_path(CUSTOM_SCOPE);
    }

    #[test]
    fn dynamic_model_metadata_synthesizes_explicit_plan_models() {
        let _lock = test_lock();
        init_version();
        let model = dynamic_model_metadata("gpt-5.9-codex").unwrap();
        assert_eq!(model.tier, ModelTier::Strong);
        assert_eq!(model.max_output_tokens, 128_000);
        assert_eq!(model.context_window, 272_000);
    }

    #[test]
    fn dynamic_model_metadata_rejects_unrelated_o_prefix() {
        let _lock = test_lock();
        init_version();
        assert!(dynamic_model_metadata("other-model").is_none());
    }
}
