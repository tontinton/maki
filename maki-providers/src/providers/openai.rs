use std::borrow::Cow;
use std::sync::{Mutex, OnceLock};

use crate::AgentError;

pub(crate) const PLAN_CONFIG_NOT_INITIALIZED_ERROR: &str = concat!(
    "OpenAI Coding Plan config not initialized: ",
    "call set_openai_plan_codex_cli_version() before using the provider"
);

pub(crate) mod api;
pub(crate) mod auth_state;
pub(crate) mod plan;
pub(crate) mod plan_models;

pub(crate) use api::OpenAi;
pub(crate) use plan::OpenAiCodingPlan;

pub(crate) fn effective_system<'a>(
    system_prefix: &'a Option<String>,
    system: &'a str,
) -> Cow<'a, str> {
    if let Some(prefix) = system_prefix {
        Cow::Owned(format!("{prefix}\n\n{system}"))
    } else {
        Cow::Borrowed(system)
    }
}

fn plan_codex_cli_version_slot() -> &'static Mutex<Option<String>> {
    static PLAN_CODEX_CLI_VERSION: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    PLAN_CODEX_CLI_VERSION.get_or_init(|| Mutex::new(None))
}

pub fn set_plan_codex_cli_version(version: &str) {
    let mut current = plan_codex_cli_version_slot().lock().unwrap();
    *current = Some(version.trim().to_string()).filter(|value| !value.is_empty());
}

pub(crate) fn plan_codex_cli_version() -> Result<String, AgentError> {
    let current = plan_codex_cli_version_slot().lock().unwrap();
    current.clone().ok_or_else(|| AgentError::Config {
        message: PLAN_CONFIG_NOT_INITIALIZED_ERROR.into(),
    })
}

#[cfg(test)]
pub(crate) fn reset_plan_codex_cli_version() {
    let mut current = plan_codex_cli_version_slot().lock().unwrap();
    *current = None;
}

#[cfg(test)]
pub(crate) fn plan_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}
