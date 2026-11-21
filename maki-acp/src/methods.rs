use agent_client_protocol_schema::ProtocolVersion;
use agent_client_protocol_schema::v1::{
    AgentCapabilities, Implementation, InitializeResponse, LoadSessionResponse, McpCapabilities,
    NewSessionResponse, PromptCapabilities, SessionCapabilities, SessionCloseCapabilities,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionListCapabilities, SessionMode, SessionModeId, SessionModeState,
};

use maki_lua::PermissionModeInfo;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const MODE_BUILD: &str = "build";
const MODE_PLAN: &str = "plan";

pub const MODEL_CONFIG_ID: &str = "model";
pub const PERMISSIONS_CONFIG_ID: &str = "permissions";
pub const PERMISSION_ASK: &str = "ask";
pub const PERMISSION_YOLO: &str = "yolo";
const PERMISSION_ASK_DESCRIPTION: &str = "Prompt for each call that needs permission";
const PERMISSION_YOLO_DESCRIPTION: &str = "Allow every call except the ones a deny rule blocks";

pub fn initialize_response() -> InitializeResponse {
    InitializeResponse::new(ProtocolVersion::V1)
        .agent_capabilities(
            AgentCapabilities::new()
                .load_session(true)
                .prompt_capabilities(PromptCapabilities::new().image(true).embedded_context(true))
                .mcp_capabilities(McpCapabilities::new().http(true))
                .session_capabilities(
                    SessionCapabilities::new()
                        .list(SessionListCapabilities::new())
                        .close(SessionCloseCapabilities::new()),
                ),
        )
        .auth_methods(vec![])
        .agent_info(Implementation::new("maki", VERSION))
}

pub fn mode_state(current: &str) -> SessionModeState {
    SessionModeState::new(
        SessionModeId::from(current.to_string()),
        vec![
            SessionMode::new(SessionModeId::from(MODE_BUILD.to_string()), "Build"),
            SessionMode::new(SessionModeId::from(MODE_PLAN.to_string()), "Plan"),
        ],
    )
}

pub fn new_session_response(session_id: &str) -> NewSessionResponse {
    NewSessionResponse::new(session_id.to_string()).modes(mode_state(MODE_BUILD))
}

pub fn load_session_response() -> LoadSessionResponse {
    LoadSessionResponse::new().modes(mode_state(MODE_BUILD))
}

pub fn model_config_option(current: &str, specs: &[String]) -> SessionConfigOption {
    let mut options: Vec<SessionConfigSelectOption> = specs
        .iter()
        .map(|spec| SessionConfigSelectOption::new(spec.clone(), spec.clone()))
        .collect();
    if !specs.iter().any(|spec| spec == current) {
        options.insert(
            0,
            SessionConfigSelectOption::new(current.to_string(), current.to_string()),
        );
    }
    SessionConfigOption::select(MODEL_CONFIG_ID, "Model", current.to_string(), options)
        .category(SessionConfigOptionCategory::Model)
}

/// `ask` and `yolo` are the built-in levels, the rest come from plugins.
pub fn permissions_config_option(
    current: &str,
    plugin_modes: &[PermissionModeInfo],
) -> SessionConfigOption {
    let mut options = vec![
        SessionConfigSelectOption::new(PERMISSION_ASK, "Ask")
            .description(PERMISSION_ASK_DESCRIPTION),
        SessionConfigSelectOption::new(PERMISSION_YOLO, "Yolo")
            .description(PERMISSION_YOLO_DESCRIPTION),
    ];
    options.extend(plugin_modes.iter().map(|mode| {
        SessionConfigSelectOption::new(mode.id.clone(), mode.name.clone())
            .description(mode.description.clone())
    }));
    if !options.iter().any(|o| o.value.0.as_ref() == current) {
        options.insert(
            0,
            SessionConfigSelectOption::new(current.to_string(), current.to_string()),
        );
    }
    SessionConfigOption::select(
        PERMISSIONS_CONFIG_ID,
        "Permissions",
        current.to_string(),
        options,
    )
}

pub fn is_builtin_level(level: &str) -> bool {
    level == PERMISSION_ASK || level == PERMISSION_YOLO
}

pub fn mode_id_to_agent_mode(mode_id: &str) -> Option<maki_agent::AgentMode> {
    match mode_id {
        MODE_BUILD => Some(maki_agent::AgentMode::Build),
        MODE_PLAN => {
            let storage = maki_storage::StateDir::resolve().ok()?;
            let plan_path = maki_storage::plans::new_plan_path(&storage).ok()?;
            Some(maki_agent::AgentMode::Plan(plan_path))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    fn judge() -> PermissionModeInfo {
        PermissionModeInfo {
            id: "judge".to_owned(),
            name: "Judge".to_owned(),
            description: None,
        }
    }

    fn values(option: &SessionConfigOption) -> Vec<String> {
        let json = serde_json::to_value(option).unwrap();
        json["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test_case(&[], PERMISSION_ASK, &["ask", "yolo"] ; "builtins_only")]
    #[test_case(&[judge()], "judge", &["ask", "yolo", "judge"] ; "a_plugin_level_follows_the_builtins")]
    #[test_case(&[], "gone", &["gone", "ask", "yolo"] ; "a_level_no_plugin_offers_any_more_stays_selectable")]
    fn permissions_option_lists_what_is_selectable(
        plugin_modes: &[PermissionModeInfo],
        current: &str,
        expected: &[&str],
    ) {
        let option = permissions_config_option(current, plugin_modes);

        assert_eq!(values(&option), expected);
        let json = serde_json::to_value(&option).unwrap();
        assert_eq!(json["id"], PERMISSIONS_CONFIG_ID);
        assert_eq!(json["currentValue"], current);
        assert_ne!(
            json["category"], "model",
            "a client reads the model option as its model picker"
        );
    }
}
