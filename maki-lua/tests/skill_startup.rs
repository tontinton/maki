use std::fs;
use std::sync::Arc;

use maki_agent::AgentMode;
use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::stub_ctx;
use maki_lua::{PluginHost, UiAction};
use serde_json::json;

const SKILL_SOURCE: &str = include_str!("../../plugins/skill/init.lua");
const SKIPPED_PREFIX: &str = "Skipped skill: ";
const MISSING_DESCRIPTION: &str = "description must be a non-empty string";
const INVALID_MAPPING: &str = "frontmatter must be a mapping";

#[test]
fn startup_reports_each_skipped_file_and_keeps_valid_skills() {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in [
        ("missing", "---\nname: missing\n---\nInstructions"),
        ("broken", "---\n123\n---\nInstructions"),
        (
            "valid",
            "---\nname: valid\ndescription: Example\n---\nInstructions",
        ),
    ] {
        let path = dir.path().join("skills").join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("SKILL.md"), content).unwrap();
    }
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    let source = format!(
        "maki.uv.cwd = function() return nil end\nmaki.uv.os_homedir = function() return nil end\nmaki.env.config_dir = function() return {} end\n{}",
        serde_json::to_string(dir.path()).unwrap(),
        SKILL_SOURCE,
    );
    host.load_source_with_opts(
        "skill",
        &source,
        json!({"plugin_dev": false}).as_object().unwrap().clone(),
    )
    .unwrap();
    let tool = registry.get("skill").unwrap();
    let invocation = tool.tool.parse(&json!({"name": "valid"})).unwrap();
    let result = smol::block_on(invocation.execute(&stub_ctx(&AgentMode::Build)));
    assert!(result.output.is_ok());
    let notices: Vec<_> = host
        .ui_action_rx()
        .drain()
        .map(|action| {
            let UiAction::Flash(message) = action else {
                panic!("skipped skills must produce UI notices");
            };
            message
        })
        .collect();
    assert_eq!(notices.len(), 2);
    for (name, reason) in [
        ("broken", INVALID_MAPPING),
        ("missing", MISSING_DESCRIPTION),
    ] {
        let path = dir.path().join("skills").join(name).join("SKILL.md");
        assert!(
            notices
                .iter()
                .any(|notice| notice == &format!("{SKIPPED_PREFIX}{}: {reason}", path.display()))
        );
    }
}
