use std::path::{Path, PathBuf};
use std::sync::Arc;

use maki_config::Effect;
use maki_providers::{Model, RequestOptions, ThinkingConfig, TokenUsage, settle_session};
use maki_storage::StateDir;
use maki_storage::sessions::{SessionClaim, StoredEffect, StoredMode, StoredRule};

use crate::{AppSession, OpenSession};

use super::mode::{Mode, PlanState};

pub(crate) struct SessionState {
    /// Shared with the writer thread, so a checkpoint is just a refcount bump.
    pub session: Arc<AppSession>,
    /// The right to write [`Self::session`]. Every queued snapshot carries a
    /// clone, so swapping this out frees the old session once its last
    /// snapshot lands.
    pub claim: SessionClaim,
    pub model: Model,
    pub token_usage: TokenUsage,
    /// What the session has billed so far: the restored total plus every turn
    /// since. Kept running, because re-deriving it from the counters would
    /// re-price history at today's rates. `None` while nothing was priced.
    pub cost: Option<f64>,
    /// Sum of what subsidised turns in this session would have cost at the
    /// provider's published list price: the total restored from the session
    /// file plus every subsidised turn since. `None` until a subsidised turn
    /// lands; unaffected by ordinary, per-token-billed turns.
    pub subsidised_list_cost: Option<f64>,
    pub context_size: u32,
    pub mode: Mode,
    pub plan: PlanState,
    pub warnings: Vec<String>,
    pub thinking: ThinkingConfig,
    /// What we actually bill and send.
    pub fast: bool,
    /// A wish parked until discovery answers, so a `/fast` typed while the
    /// model list is still loading is not thrown on the floor.
    pub pending_fast: bool,
    pub workflow: bool,
}

const PLAN_FILE_MISSING_WARNING: &str = "Plan file was deleted \u{2014} started a new plan";

/// The badge, the cost line and the request all read the same gate, so none of
/// them can advertise a mode this model lacks, or miss one it demands. Fast
/// comes back split into "on now" and "still waiting", which is the only place
/// those two bits are derived.
fn clamp(thinking: ThinkingConfig, fast: bool, model: &Model) -> (ThinkingConfig, bool, bool) {
    let opts = RequestOptions { thinking, fast }.clamped(model);
    (opts.thinking, opts.fast, fast && model.fast_pending())
}

impl SessionState {
    /// The caller already resolved this model against the policy and the
    /// provider. Deciding again here is exactly how the app and the agent used
    /// to drift apart, so this adopts what it is handed and stays the only
    /// writer of `session.model`. Drawn, sent and stored then agree for free.
    pub fn from_session(open: OpenSession, model: &Model, storage: &StateDir) -> Self {
        let OpenSession { mut session, claim } = open;
        session.set_model(model.spec());
        let model = model.clone();

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

        // Saved model may differ from the live one (updated, removed, etc), so
        // reconcile before anyone reads the toggles or prices history with them.
        let (thinking, fast, pending_fast) =
            clamp(session.meta.thinking.into(), session.meta.fast, &model);
        let token_usage = session.token_usage;
        let cost = settle_session(&token_usage, session.usage_by_model_mut(), &model, fast);
        // Unlike `cost` there is nothing to settle: the list price was
        // recorded per turn and never moves, so resuming just adds the rows
        // back up. Without it a resumed subsidised session reads `$0.000`
        // with no reference figure until the next turn lands.
        let subsidised_list_cost = session
            .usage_by_model()
            .values()
            .filter_map(|usage| usage.subsidised_list_cost)
            .reduce(|total, cost| total + cost);
        let context_size = session.meta.context_size;

        Self {
            thinking,
            fast,
            pending_fast,
            workflow: session.meta.workflow,
            session: Arc::new(session),
            claim,
            model,
            token_usage,
            cost,
            subsidised_list_cost,
            context_size,
            mode,
            plan,
            warnings,
        }
    }

    pub fn session_mut(&mut self) -> &mut AppSession {
        Arc::make_mut(&mut self.session)
    }

    /// What the user asked for, whether or not the model can honour it yet.
    /// This is the bit that gets persisted.
    pub fn fast_intent(&self) -> bool {
        self.fast || self.pending_fast
    }

    pub fn set_fast(&mut self, fast: bool) {
        (self.thinking, self.fast, self.pending_fast) = clamp(self.thinking, fast, &self.model);
    }

    pub fn update_model(&mut self, model: &Model) {
        (self.thinking, self.fast, self.pending_fast) =
            clamp(self.thinking, self.fast_intent(), model);
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
    use maki_providers::model::FastSupport;
    use maki_providers::{FastPricing, ModelPricing, ThinkingSupport};
    use maki_storage::sessions::{Effort, SessionClaim, SessionError, SessionLog, StoredThinking};
    use test_case::test_case;

    const RECORDED_COST: f64 = 0.42;
    /// What a subsidised turn would have billed; the turn itself billed `$0`.
    const RECORDED_LIST_COST: f64 = 1.75;
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
    const THINKING_NOT_LIFTED: &str =
        "a model that requires thinking must not show, or send, thinking off";
    const USAGE_REATTRIBUTED: &str =
        "usage earned under another model must stay keyed to it, not move onto the adopted one";
    const CURSOR_VOIDED_FOR_NOTHING: &str =
        "adopting the model the session already runs on changes no header, so the cursor must live";
    const OLD_SPEC_STAYS_ON_DISK: &str = "the model lives in the header record, which only a rewrite touches, so adopting a new one must void the append cursor";
    const APPEND_FAILED: &str = "append failed for an unrelated reason";

    fn resumed(session: AppSession, model: &Model) -> SessionState {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        SessionState::from_session(OpenSession::claimed(session, &storage), model, &storage)
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

    /// The list price is written per model and never re-derived, so a resumed
    /// subsidised session has to add the stored rows back up. Dropping it left
    /// the status bar on a bare `$0.000` until the next turn landed.
    #[test]
    fn resumed_session_restores_the_recorded_list_price() {
        let mut session = session_with_counters();
        session.add_model_usage(
            UNRESOLVABLE_MODEL,
            session
                .token_usage
                .billed_with_subsidised_list_cost(Some(0.0), Some(RECORDED_LIST_COST)),
        );
        let state = resumed(session, &test_model());
        assert_eq!(state.subsidised_list_cost, Some(RECORDED_LIST_COST));
    }

    /// Nothing subsidised ever ran, so there is no reference figure to show
    /// and the status bar must not invent one.
    #[test]
    fn resumed_metered_session_has_no_list_price() {
        let mut session = session_with_counters();
        session.add_model_usage(
            UNRESOLVABLE_MODEL,
            session.token_usage.billed(Some(RECORDED_COST)),
        );
        assert_eq!(resumed(session, &test_model()).subsidised_list_cost, None);
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

    #[test_case(FastSupport::Pending, false, true ; "pending_preserves_saved_intent")]
    #[test_case(FastSupport::Supported, true, false ; "supported_restores_fast")]
    #[test_case(FastSupport::Unsupported, false, false ; "unsupported_clears_saved_intent")]
    fn resume_fast_support(support: FastSupport, fast: bool, pending: bool) {
        let mut session = session_with_counters();
        session.meta.fast = true;
        let mut model = test_model();
        model.supports_fast_override = Some(support);
        model.pricing.fast = Some(FastPricing {
            input: FAST_INPUT_RATE,
            output: test_pricing().output,
        });
        let state = resumed(session, &model);
        assert_eq!((state.fast, state.pending_fast), (fast, pending));
        assert_eq!(
            state.cost,
            Some(if fast { FAST_INPUT_RATE } else { LIST_PRICE })
        );
    }

    #[test_case(FastSupport::Supported, false, true, false ; "discovery_enables_fast")]
    #[test_case(FastSupport::Unsupported, false, false, false ; "discovery_rejects_fast")]
    #[test_case(FastSupport::Pending, false, false, true ; "still_pending_keeps_intent")]
    #[test_case(FastSupport::Supported, true, true, false ; "switch_keeps_pending_intent")]
    fn pending_fast_model_update(support: FastSupport, switch: bool, fast: bool, pending: bool) {
        let mut session = session_with_counters();
        session.meta.fast = true;
        let mut model = test_model();
        model.supports_fast_override = Some(FastSupport::Pending);
        let mut state = resumed(session, &model);
        assert_eq!((state.fast, state.pending_fast), (false, true));
        if switch {
            model.id = UNRESOLVABLE_MODEL.into();
        }
        model.supports_fast_override = Some(support);
        state.update_model(&model);
        assert_eq!((state.fast, state.pending_fast), (fast, pending));
    }

    #[test]
    fn plan_mode_without_path_allocates_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session = make_plan_session(Some(StoredMode::Plan), None);
        let state = SessionState::from_session(
            OpenSession::claimed(session, &storage),
            &test_model(),
            &storage,
        );
        assert_eq!(state.mode, Mode::Plan);
        assert!(state.plan.path().is_some(), "plan path should be allocated");
    }

    #[test]
    fn plan_mode_with_missing_file_allocates_new_path_and_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session =
            make_plan_session(Some(StoredMode::Plan), Some("/nonexistent/plan.md".into()));
        let state = SessionState::from_session(
            OpenSession::claimed(session, &storage),
            &test_model(),
            &storage,
        );
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
        let state = SessionState::from_session(
            OpenSession::claimed(session, &storage),
            &test_model(),
            &storage,
        );
        assert_eq!(state.mode, Mode::Plan);
        assert_eq!(state.plan.path(), Some(plan_file.as_path()));
    }

    #[test]
    fn from_session_adopts_the_model_it_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let resolved = test_model();
        let mut session = make_plan_session(Some(StoredMode::Build), None);
        session.model = "openai/gpt-5".into();

        let state = SessionState::from_session(
            OpenSession::claimed(session, &storage),
            &resolved,
            &storage,
        );

        assert_eq!(state.model.spec(), resolved.spec());
        assert_eq!(state.session.model, resolved.spec());
    }

    #[test]
    fn build_mode_does_not_allocate_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session = make_plan_session(Some(StoredMode::Build), None);
        let state = SessionState::from_session(
            OpenSession::claimed(session, &storage),
            &test_model(),
            &storage,
        );
        assert_eq!(state.mode, Mode::Build);
        assert!(state.plan.path().is_none());
    }

    /// The level `clamped` picks is its own business. All this layer promises
    /// is that it asks, instead of keeping its own rule that only knew how to
    /// turn thinking off.
    #[test]
    fn update_model_lifts_thinking_for_a_model_that_requires_it() {
        let mut state = resumed(AppSession::new("test-model", "/tmp"), &test_model());
        state.thinking = ThinkingConfig::Off;

        let mut required = test_model();
        required.thinking_override = Some(ThinkingSupport::Required);
        state.update_model(&required);

        assert_ne!(state.thinking, ThinkingConfig::Off, "{THINKING_NOT_LIFTED}");
    }

    /// Resume no longer re-derives the model, so the stored toggle now meets
    /// whatever the caller resolved with no `adjust_model` behind it to paper
    /// over a mismatch. A model that cannot think has to silence the toggle,
    /// one that can has to keep it.
    #[test_case(StoredThinking::Adaptive, ThinkingSupport::No => ThinkingConfig::Off ; "unsupported_silences_adaptive")]
    #[test_case(StoredThinking::Effort { level: Effort::High }, ThinkingSupport::No => ThinkingConfig::Off ; "unsupported_silences_effort")]
    #[test_case(StoredThinking::Adaptive, ThinkingSupport::Yes => ThinkingConfig::Adaptive ; "supported_preserves_adaptive")]
    #[test_case(StoredThinking::Effort { level: Effort::High }, ThinkingSupport::Yes => ThinkingConfig::Effort(Effort::High) ; "supported_preserves_effort")]
    fn resume_clamps_stored_thinking_to_the_adopted_model(
        stored: StoredThinking,
        support: ThinkingSupport,
    ) -> ThinkingConfig {
        let mut session = AppSession::new("test-model", "/tmp");
        session.meta.thinking = Some(stored);
        let model = Model {
            thinking_override: Some(support),
            ..test_model()
        };

        resumed(session, &model).thinking
    }

    /// Adoption overwrites `session.model`, so the per-model breakdown is the
    /// only record left of who earned what. Resuming onto a different model
    /// leaves that history where it stands instead of re-keying it.
    #[test]
    fn adopting_a_model_leaves_recorded_usage_with_the_model_that_earned_it() {
        let mut session = session_with_counters();
        session.add_model_usage(
            UNRESOLVABLE_MODEL,
            session.token_usage.billed(Some(RECORDED_COST)),
        );

        let state = resumed(session, &test_model());

        let by_model = state.session.usage_by_model();
        assert!(
            by_model.len() == 1 && by_model.contains_key(UNRESOLVABLE_MODEL),
            "{USAGE_REATTRIBUTED}"
        );
    }

    /// The writer only starts the file over when the log reports divergence, so
    /// a quiet `set_model` would leave the old spec in the header and the next
    /// `maki` would resume on it. The other direction costs as well, since
    /// voiding the cursor for an unchanged spec buys a rewrite on every resume.
    #[test_case(false ; "same_model_keeps_the_append_cursor")]
    #[test_case(true ; "new_model_voids_the_append_cursor")]
    fn adopting_a_model_rewrites_the_header_only_when_the_spec_moves(adopt_other: bool) {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut model = test_model();
        let session = AppSession::new(&model.spec(), "/tmp");
        let claim = SessionClaim::acquire_in(session.id, tmp.path()).unwrap();
        let mut log = SessionLog::rewrite(tmp.path(), &claim, &session).unwrap();
        if adopt_other {
            model.id = UNRESOLVABLE_MODEL.into();
        }

        let state = SessionState::from_session(
            OpenSession {
                session,
                claim: claim.clone(),
            },
            &model,
            &storage,
        );

        match log.append(&claim, &state.session) {
            Ok(()) => assert!(!adopt_other, "{OLD_SPEC_STAYS_ON_DISK}"),
            Err(SessionError::LogDiverged { .. }) => {
                assert!(adopt_other, "{CURSOR_VOIDED_FOR_NOTHING}")
            }
            Err(e) => panic!("{APPEND_FAILED}: {e}"),
        }
    }
}
