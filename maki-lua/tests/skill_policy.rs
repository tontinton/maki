use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_lua::PluginHost;
use serde_json::json;
use test_case::test_case;

const SKILL_SOURCE: &str = include_str!("../../plugins/skill/init.lua");
const RELEASE_SOURCE: &str =
    "---\nname: release\ndescription: Cut a release\n---\nRead CHANGELOG.md first.";

fn isolated_skill_host(has_skill: bool) -> (tempfile::TempDir, Arc<ToolRegistry>, PluginHost) {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(".maki/skills/release");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    if has_skill {
        std::fs::write(skill_dir.join("SKILL.md"), RELEASE_SOURCE).unwrap();
    }
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    let source = format!(
        "maki.uv.cwd = function() return {} end\nmaki.uv.os_homedir = function() return nil end\nmaki.env.config_dir = function() return nil end\n{}",
        serde_json::to_string(dir.path()).unwrap(),
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
    let (_dir, registry, _host) = isolated_skill_host(has_skill);
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
