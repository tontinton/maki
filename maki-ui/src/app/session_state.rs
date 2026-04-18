use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use maki_agent::ToolOutput;
use maki_agent::permissions::PermissionManager;
use maki_config::Effect;
use maki_providers::{Message, Model, ThinkingConfig, TokenUsage};
use maki_storage::DataDir;
use maki_storage::sessions::{StoredEffect, StoredMode, StoredRule};

use crate::AppSession;

use super::mode::{Mode, PlanState};

pub(crate) struct SessionState {
    pub session: AppSession,
    pub model: Model,
    pub token_usage: TokenUsage,
    pub context_size: u32,
    pub mode: Mode,
    pub plan: PlanState,
    pub warnings: Vec<String>,
    pub thinking: ThinkingConfig,
}

const PLAN_FILE_MISSING_WARNING: &str = "Plan file was deleted \u{2014} started a new plan";

impl SessionState {
    pub fn from_session(
        mut session: AppSession,
        fallback_model: &Model,
        storage: &DataDir,
    ) -> Self {
        let model = Model::from_spec(&session.model).unwrap_or_else(|_| {
            session.model = fallback_model.spec();
            fallback_model.clone()
        });

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

        let token_usage = session.token_usage;
        let context_size = session.meta.context_size;

        Self {
            thinking: session.meta.thinking.map(Into::into).unwrap_or_default(),
            session,
            model,
            token_usage,
            context_size,
            mode,
            plan,
            warnings,
        }
    }

    pub fn sync_session(
        &mut self,
        shared_history: &Option<Arc<ArcSwap<Vec<Message>>>>,
        shared_tool_outputs: &Option<Arc<Mutex<HashMap<String, ToolOutput>>>>,
        permissions: &Arc<PermissionManager>,
    ) {
        if let Some(history) = shared_history {
            self.session.messages = Vec::clone(&history.load());
        }
        if let Some(outputs) = shared_tool_outputs {
            self.session.tool_outputs = outputs.lock().unwrap_or_else(|e| e.into_inner()).clone();
        }
        self.session.token_usage = self.token_usage;
        self.session.meta.context_size = self.context_size;
        self.session.meta.mode = Some(self.mode.into());
        self.session.meta.plan_path = self.plan.path().map(|p| p.to_string_lossy().into_owned());
        self.session.meta.plan_written = self.plan.is_ready();
        self.session.meta.session_rules = rules_to_stored(&permissions.session_rules_snapshot());
        self.session.meta.thinking = Some(self.thinking.into());
        self.session.updated_at = maki_storage::now_epoch();
        self.session.update_title_if_default();
    }

    pub fn update_model(&mut self, model: &Model) {
        if !model.provider.supports_thinking() {
            self.thinking = ThinkingConfig::Off;
        }
        self.session.model = model.spec();
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
        .map(|r| StoredRule {
            tool: r.tool.clone(),
            scope: r.scope.clone(),
            effect: match r.effect {
                Effect::Allow => StoredEffect::Allow,
                Effect::Deny => StoredEffect::Deny,
            },
        })
        .collect()
}

pub(crate) fn stored_to_rules(stored: &[StoredRule]) -> Vec<maki_config::PermissionRule> {
    stored
        .iter()
        .map(|r| maki_config::PermissionRule {
            tool: r.tool.clone(),
            scope: r.scope.clone(),
            effect: match r.effect {
                StoredEffect::Allow => Effect::Allow,
                StoredEffect::Deny => Effect::Deny,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::test_model;

    fn make_plan_session(mode: Option<StoredMode>, plan_path: Option<String>) -> AppSession {
        let mut session = AppSession::new("test-model", "/tmp");
        session.meta.mode = mode;
        session.meta.plan_path = plan_path;
        session
    }

    #[test]
    fn plan_mode_without_path_allocates_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = DataDir::from_path(tmp.path().to_path_buf());
        let session = make_plan_session(Some(StoredMode::Plan), None);
        let state = SessionState::from_session(session, &test_model(), &storage);
        assert_eq!(state.mode, Mode::Plan);
        assert!(state.plan.path().is_some(), "plan path should be allocated");
    }

    #[test]
    fn plan_mode_with_missing_file_allocates_new_path_and_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = DataDir::from_path(tmp.path().to_path_buf());
        let session =
            make_plan_session(Some(StoredMode::Plan), Some("/nonexistent/plan.md".into()));
        let state = SessionState::from_session(session, &test_model(), &storage);
        assert_eq!(state.mode, Mode::Plan);
        let path = state.plan.path().expect("plan path should be allocated");
        assert_ne!(path, Path::new("/nonexistent/plan.md"));
        assert_eq!(state.warnings.len(), 1);
        assert_eq!(state.warnings[0], PLAN_FILE_MISSING_WARNING);
    }

    #[test]
    fn plan_mode_with_existing_file_preserves_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = DataDir::from_path(tmp.path().to_path_buf());
        let plan_file = tmp.path().join("existing-plan.md");
        std::fs::write(&plan_file, "# Plan").unwrap();
        let session = make_plan_session(
            Some(StoredMode::Plan),
            Some(plan_file.to_string_lossy().into_owned()),
        );
        let state = SessionState::from_session(session, &test_model(), &storage);
        assert_eq!(state.mode, Mode::Plan);
        assert_eq!(state.plan.path(), Some(plan_file.as_path()));
    }

    #[test]
    fn build_mode_does_not_allocate_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = DataDir::from_path(tmp.path().to_path_buf());
        let session = make_plan_session(Some(StoredMode::Build), None);
        let state = SessionState::from_session(session, &test_model(), &storage);
        assert_eq!(state.mode, Mode::Build);
        assert!(state.plan.path().is_none());
    }
}
