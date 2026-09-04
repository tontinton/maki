use std::path::{Path, PathBuf};
use std::sync::Arc;

use maki_config::{Effect, ModelPolicy};
use maki_providers::provider::adjust_model;
use maki_providers::{Model, ThinkingConfig, Timeouts, TokenUsage, settle_session};
use maki_storage::StateDir;
use maki_storage::sessions::{StoredEffect, StoredMode, StoredRule};

use crate::AppSession;

use super::mode::{Mode, PlanState};

pub(crate) struct SessionState {
    /// Shared with the writer thread, so a checkpoint is just a refcount bump.
    pub session: Arc<AppSession>,
    pub model: Model,
    pub token_usage: TokenUsage,
    /// What the session has billed so far: the restored total plus every turn
    /// since. Kept running, because re-deriving it from the counters would
    /// re-price history at today's rates. `None` while nothing was priced.
    pub cost: Option<f64>,
    /// Sum of what subsidised turns in this session would have cost at the
    /// provider's published list price. `None` until a subsidised turn lands;
    /// unaffected by ordinary, per-token-billed turns.
    pub list_cost: Option<f64>,
    pub context_size: u32,
    pub mode: Mode,
    pub plan: PlanState,
    pub warnings: Vec<String>,
    pub thinking: ThinkingConfig,
    pub fast: bool,
    pub workflow: bool,
}

const PLAN_FILE_MISSING_WARNING: &str = "Plan file was deleted \u{2014} started a new plan";

impl SessionState {
    pub fn from_session(
        mut session: AppSession,
        fallback_model: &Model,
        storage: &StateDir,
        model_policy: &ModelPolicy,
    ) -> Self {
        let mut model = model_policy
            .allows(&session.model)
            .then(|| Model::from_spec(&session.model))
            .transpose()
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                session.model = fallback_model.spec();
                fallback_model.clone()
            });
        // Apply the provider's per-model adjustments (e.g. ZAI's glm-5.2
        // thinking support, or Aperture's routed-provider inheritance) so a
        // resumed session matches one started fresh.
        if let Err(e) = adjust_model(&mut model, Timeouts::default()) {
            tracing::warn!(model = %model.id, error = %e, "failed to adjust resumed model");
        }

        let mode = match session.meta.mode {
            Some(StoredMode::Plan) => Mode::Plan,
            _ => Mode::Build,
        };

        let mut warnings = Vec::new();

        let mut plan = match &session.meta.plan_path {
            Some(p) if Path::new(p).exists() => {
                if session.meta.plan_written {
                    PlanState::Ready(PathBuf::from(p))
                } else {
                    PlanState::Drafting(PathBuf::from(p))
                }
            }
            Some(_) => {
                warnings.push(PLAN_FILE_MISSING_WARNING.into());
                PlanState::None
            }
            None => PlanState::None,
        };

        if mode == Mode::Plan {
            plan.allocate_path(storage);
        }

        let fast = session.meta.fast && model.supports_fast();
        let token_usage = session.token_usage;
        let cost = settle_session(&token_usage, session.usage_by_model_mut(), &model, fast);
        let context_size = session.meta.context_size;

        Self {
            // Saved model may differ from the live one (updated, removed, etc).
            // Reconcile so the UI badge and agent always see the truth.
            thinking: session
                .meta
                .thinking
                .map(Into::into)
                .filter(|_| model.supports_thinking())
                .unwrap_or_default(),
            fast,
            workflow: session.meta.workflow,
            session: Arc::new(session),
            model,
            token_usage,
            cost,
            // Not settled from storage like `cost`: subsidised sessions are
            // new enough that resuming one always re-derives it from the
            // per-turn stream rather than trusting old on-disk totals.
            list_cost: None,
            context_size,
            mode,
            plan,
            warnings,
        }
    }

    pub fn session_mut(&mut self) -> &mut AppSession {
        Arc::make_mut(&mut self.session)
    }

    pub fn update_model(&mut self, model: &Model) {
        if !model.supports_thinking() {
            self.thinking = ThinkingConfig::Off;
        }
        if !model.supports_fast() {
            self.fast = false;
        }
        self.session_mut().set_model(model.spec());
        self.model = model.clone();
    }
}

impl From<Mode> for StoredMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Build => StoredMode::Build,
            Mode::Plan => StoredMode::Plan,
        }
    }
}

pub(crate) fn rules_to_stored(rules: &[maki_config::PermissionRule]) -> Vec<StoredRule> {
    rules
        .iter()
        .map(|r| {
            let effect = match r.effect {
                Effect::Allow => StoredEffect::Allow,
                Effect::Deny => StoredEffect::Deny,
            };
            StoredRule {
                tool: r.tool.to_string(),
                scope: r.scope.clone(),
                effect,
            }
        })
        .collect()
}

/// Migrate old stored tool key formats to `ToolKey`.
/// Handles `"mcp:server__tool"` (pre-PR1 format) -> `McpTool`.
/// All other formats go through `ToolKey::parse` (current format: `server.tool`).
fn migrate_stored_tool_key(s: &str) -> Option<maki_config::ToolKey> {
    // Pre-PR1 format: "mcp:server__tool" — rewrite to new format and parse.
    if let Some(rest) = s.strip_prefix("mcp:")
        && let Some((server, tool)) = rest.split_once("__")
    {
        let new_form = format!("{server}.{tool}");
        return maki_config::ToolKey::parse(&new_form)
            .map_err(
                |e| tracing::warn!(key = s, error = %e, "malformed stored tool key — skipping"),
            )
            .ok();
    }
    match maki_config::ToolKey::parse(s) {
        Ok(key) => Some(key),
        Err(e) => {
            tracing::error!(key = s, error = %e, "malformed stored tool key — rule DROPPED; a deny rule may have been lost");
            None
        }
    }
}

pub(crate) fn stored_to_rules(stored: &[StoredRule]) -> Vec<maki_config::PermissionRule> {
    stored
        .iter()
        .filter_map(|r| {
            let tool = match migrate_stored_tool_key(&r.tool) {
                Some(t) => t,
                None => {
                    if matches!(r.effect, StoredEffect::Deny) {
                        tracing::error!(
                            key = %r.tool,
                            "SECURITY: stored DENY rule dropped — tool may now be accessible. \
                             Re-add this rule manually in permissions.toml"
                        );
                    }
                    return None;
                }
            };
            let effect = match r.effect {
                StoredEffect::Allow => Effect::Allow,
                StoredEffect::Deny => Effect::Deny,
            };
            Some(maki_config::PermissionRule {
                tool,
                scope: r.scope.clone(),
                effect,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{test_model, test_pricing};
    use maki_providers::{FastPricing, ModelPricing};
    use maki_storage::sessions::StoredThinking;
    use test_case::test_case;

    const RECORDED_COST: f64 = 0.42;
    /// A round million, so a per-million rate reads straight off the bill.
    const MILLION_INPUT: TokenUsage = TokenUsage {
        input: 1_000_000,
        output: 0,
        cache_creation: 0,
        cache_read: 0,
        cost: None,
    };
    /// [`MILLION_INPUT`] at `test_pricing`'s standard input rate.
    const LIST_PRICE: f64 = 3.0;
    /// Twice the standard rate, so a resume that ignores `fast` bills half.
    const FAST_INPUT_RATE: f64 = 6.0;
    const UNRESOLVABLE_MODEL: &str = "a-model-no-table-has-ever-heard-of";
    const FAST_FLAG_LOST: &str = "the model has fast pricing, so the flag must survive as stored";

    fn resumed(session: AppSession, model: &Model) -> SessionState {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        SessionState::from_session(session, model, &storage, &ModelPolicy::default())
    }

    /// An old session: counters, no per-model breakdown.
    fn session_with_counters() -> AppSession {
        let mut session = AppSession::new("test-model", "/tmp");
        session.token_usage = MILLION_INPUT;
        session
    }

    fn make_plan_session(mode: Option<StoredMode>, plan_path: Option<String>) -> AppSession {
        let mut session = AppSession::new("test-model", "/tmp");
        session.meta.mode = mode;
        session.meta.plan_path = plan_path;
        session
    }

    /// A resumed session opens on the bill it ran up, not on its counters
    /// re-priced with whatever model is selected now. The model that recorded
    /// this one prices to nothing, so only the recorded cost can answer.
    #[test]
    fn resumed_session_opens_on_the_cost_its_turns_recorded() {
        let mut session = session_with_counters();
        session.add_model_usage(
            UNRESOLVABLE_MODEL,
            session.token_usage.billed(Some(RECORDED_COST)),
        );
        let state = resumed(session, &test_model());
        assert_eq!(state.cost, Some(RECORDED_COST));
    }

    /// Older sessions kept counters only, and those are priced with the
    /// session's own clamped `fast` flag. A hardcoded `false` would open a
    /// resumed fast session on half its bill.
    #[test_case(false => Some(LIST_PRICE)     ; "standard_rates")]
    #[test_case(true  => Some(FAST_INPUT_RATE) ; "fast_rates")]
    fn resume_without_a_breakdown_prices_the_counters(fast: bool) -> Option<f64> {
        let mut session = session_with_counters();
        session.meta.fast = fast;
        let model = Model {
            pricing: ModelPricing {
                fast: Some(FastPricing {
                    input: FAST_INPUT_RATE,
                    output: test_pricing().output,
                }),
                ..test_pricing()
            },
            ..test_model()
        };

        let state = resumed(session, &model);

        assert_eq!(state.fast, fast, "{FAST_FLAG_LOST}");
        state.cost
    }

    #[test]
    fn plan_mode_without_path_allocates_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session = make_plan_session(Some(StoredMode::Plan), None);
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert_eq!(state.mode, Mode::Plan);
        assert!(state.plan.path().is_some(), "plan path should be allocated");
    }

    #[test]
    fn plan_mode_with_missing_file_allocates_new_path_and_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session =
            make_plan_session(Some(StoredMode::Plan), Some("/nonexistent/plan.md".into()));
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert_eq!(state.mode, Mode::Plan);
        let path = state.plan.path().expect("plan path should be allocated");
        assert_ne!(path, Path::new("/nonexistent/plan.md"));
        assert_eq!(state.warnings.len(), 1);
        assert_eq!(state.warnings[0], PLAN_FILE_MISSING_WARNING);
    }

    #[test]
    fn plan_mode_with_existing_file_preserves_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let plan_file = tmp.path().join("existing-plan.md");
        std::fs::write(&plan_file, "# Plan").unwrap();
        let session = make_plan_session(
            Some(StoredMode::Plan),
            Some(plan_file.to_string_lossy().into_owned()),
        );
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert_eq!(state.mode, Mode::Plan);
        assert_eq!(state.plan.path(), Some(plan_file.as_path()));
    }

    #[test]
    fn disallowed_restored_model_uses_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let fallback = test_model();
        let mut session = make_plan_session(Some(StoredMode::Build), None);
        session.model = "openai/gpt-5".into();
        let raw: maki_config::RawConfig = serde_json::from_value(serde_json::json!({
            "provider": {"allowed_models": [fallback.spec()]}
        }))
        .unwrap();
        let policy = raw.into_config(&[]).unwrap().provider.model_policy;

        let state = SessionState::from_session(session, &fallback, &storage, &policy);

        assert_eq!(state.model.spec(), fallback.spec());
        assert_eq!(state.session.model, fallback.spec());
    }

    #[test]
    fn build_mode_does_not_allocate_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session = make_plan_session(Some(StoredMode::Build), None);
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert_eq!(state.mode, Mode::Build);
        assert!(state.plan.path().is_none());
    }

    #[test]
    fn from_session_applies_provider_adjust_model() {
        // SAFETY: this test runs single-threaded; no other thread reads the env.
        unsafe { std::env::set_var("APERTURE_HOST", "https://example.com") };
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut session = AppSession::new("aperture/zai/glm-5.2", "/tmp");
        session.meta.thinking = Some(StoredThinking::Adaptive);
        let state =
            SessionState::from_session(session, &test_model(), &storage, &ModelPolicy::default());
        assert!(
            state.model.supports_thinking(),
            "resumed aperture/zai/glm-5.2 should inherit thinking support from adjust_model",
        );
        assert_eq!(
            state.thinking,
            ThinkingConfig::Adaptive,
            "resumed thinking config should be preserved when the model supports it",
        );
    }
}
