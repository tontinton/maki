use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_lua::PluginHost;
use test_case::test_case;

#[test_case("edit", include_str!("../../plugins/edit/tests/spec.lua") ; "edit_plugin_spec")]
#[test_case("index", include_str!("../../plugins/index/tests/spec.lua") ; "index_plugin_spec")]
#[test_case("lib", include_str!("../../plugins/lib/tests/spec.lua") ; "lib_spec")]
#[test_case("memory", include_str!("../../plugins/memory/tests/spec.lua") ; "memory_plugin_spec")]
#[test_case("question", include_str!("../../plugins/question/tests/spec.lua") ; "question_plugin_spec")]
#[test_case("read", include_str!("../../plugins/read/tests/spec.lua") ; "read_plugin_spec")]
#[test_case("skill", include_str!("../../plugins/skill/tests/spec.lua") ; "skill_plugin_spec")]
#[test_case("task", include_str!("../../plugins/task/tests/spec.lua") ; "task_plugin_spec")]
#[test_case("view_image", include_str!("../../plugins/view_image/tests/spec.lua") ; "view_image_plugin_spec")]
#[test_case("webfetch", include_str!("../../plugins/webfetch/tests/spec.lua") ; "webfetch_plugin_spec")]
#[test_case("websearch", include_str!("../../plugins/websearch/tests/spec.lua") ; "websearch_plugin_spec")]
#[test_case("write", include_str!("../../plugins/write/tests/spec.lua") ; "write_plugin_spec")]
fn plugin_spec(name: &str, spec: &str) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(&format!("{name}_spec"), spec)
        .unwrap_or_else(|e| panic!("{name} spec failed:\n{e}"));
}
