use std::fs;
use std::sync::Arc;

use maki_agent::AgentMode;
use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::stub_ctx;
use maki_config::ProjectConfig;
use maki_config::project::{self, TrustMode};
use maki_lua::PluginHost;
use maki_lua::test_support::runtime;
use serde_json::json;
use tempfile::TempDir;
use test_case::test_case;

const SKILL_SOURCE: &str = include_str!("../../plugins/skill/init.lua");
const RELEASE_SOURCE: &str =
    "---\nname: release\ndescription: Cut a release\n---\nRead CHANGELOG.md first.";
const SKILL_NOT_FOUND: &str = "skill not found: ";

fn isolated_skill_host(
    has_skill: bool,
    trusted: bool,
    relative: &str,
) -> (TempDir, Arc<ToolRegistry>, PluginHost) {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(relative).join("release");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    if has_skill {
        fs::write(skill_dir.join("SKILL.md"), RELEASE_SOURCE).unwrap();
    }
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    if trusted {
        let decision = project::resolve_noninteractive(dir.path(), TrustMode::Session);
        host.set_project_config(&decision.project_config).unwrap();
    }
    let source = format!(
        "maki.uv.cwd = function() return {} end\nmaki.uv.os_homedir = function() return nil end\nmaki.env.config_dir = function() return {} end\n{}",
        serde_json::to_string(dir.path()).unwrap(),
        serde_json::to_string(&dir.path().join("personal")).unwrap(),
        SKILL_SOURCE,
    );
    host.load_source_with_opts(
        "skill",
        &source,
        json!({ "plugin_dev": false }).as_object().unwrap().clone(),
    )
    .unwrap();
    (dir, registry, host)
}

#[test_case(true ; "names_are_constrained_to_the_catalog")]
#[test_case(false ; "empty_catalog_has_no_tool")]
fn skill_tool_matches_the_available_catalog(has_skill: bool) {
    let (_dir, registry, _host) = isolated_skill_host(has_skill, true, ".maki/skills");
    match registry.get("skill") {
        Some(entry) if has_skill => {
            assert_eq!(
                entry.tool.schema()["properties"]["name"]["enum"],
                json!(["release"])
            );
        }
        None if !has_skill => {}
        _ => panic!("the skill tool must match the available catalog"),
    }
}

#[test_case(".maki/skills" ; "maki")]
#[test_case(".claude/skills" ; "claude")]
#[test_case(".opencode/skills" ; "opencode")]
#[test_case(".agents/skills" ; "agents")]
fn all_project_skill_aliases_require_the_same_trust(relative: &str) {
    for trusted in [false, true] {
        let (_dir, registry, _host) = isolated_skill_host(true, trusted, relative);
        assert_eq!(registry.get("skill").is_some(), trusted);
    }
}

#[test]
fn revoked_project_trust_reaches_existing_plugin_globals() {
    let (dir, registry, host) = isolated_skill_host(true, true, ".maki/skills");
    host.set_project_config(&ProjectConfig::discover(dir.path()))
        .unwrap();
    let entry = registry.get("skill").unwrap();
    let invocation = entry.tool.parse(&json!({ "name": "release" })).unwrap();
    let result = runtime().block_on(invocation.execute(&stub_ctx(&AgentMode::Build)));
    assert_eq!(
        result.output.unwrap_err(),
        format!("{SKILL_NOT_FOUND}release")
    );
}

#[test]
fn personal_skills_remain_available_without_project_trust() {
    let (_dir, registry, _host) = isolated_skill_host(true, false, "personal/skills");
    assert!(registry.get("skill").is_some());
}
