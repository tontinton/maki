use std::fs;
use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_config::ProjectConfig;
use maki_config::project::{self, TrustMode};
use maki_lua::PluginHost;
use maki_storage::paths::canonicalize_clean;
use test_case::test_case;

const PROJECT_DIRS: &str = "maki.env.project_dirs()";
const PROBE_PLUGIN: &str = "project_dirs_probe";
const GIT_DIR: &str = ".git";
const NESTED_CWD: &str = "packages/app";
const SKILL_DIR: &str = ".agents/skills";

#[test]
fn an_unconfigured_host_publishes_no_project_directories() {
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(registry).unwrap();
    host.load_source(PROBE_PLUGIN, &format!("assert(#{PROJECT_DIRS} == 0)"))
        .unwrap();
}

#[test_case(false ; "untrusted")]
#[test_case(true ; "session_trust")]
fn published_directories_follow_the_native_trust_decision(trusted: bool) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join(NESTED_CWD);
    fs::create_dir(root.path().join(GIT_DIR)).unwrap();
    fs::create_dir_all(cwd.join(SKILL_DIR)).unwrap();
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(registry).unwrap();
    let project = if trusted {
        project::resolve_noninteractive(&cwd, TrustMode::Session).project_config
    } else {
        ProjectConfig::discover(&cwd)
    };
    host.set_project_config(&project).unwrap();
    let expected = if trusted {
        vec![
            canonicalize_clean(&cwd),
            canonicalize_clean(cwd.parent().unwrap()),
            canonicalize_clean(root.path()),
        ]
    } else {
        Vec::new()
    };
    let mut source = format!(
        "local dirs = {PROJECT_DIRS}\nassert(#dirs == {})\n",
        expected.len()
    );
    for (index, path) in expected.iter().enumerate() {
        source.push_str(&format!(
            "assert(dirs[{}] == {})\n",
            index + 1,
            serde_json::to_string(path).unwrap(),
        ));
    }
    host.load_source(PROBE_PLUGIN, &source).unwrap();
}
