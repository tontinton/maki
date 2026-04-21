use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_config::PluginsConfig;
use maki_lua::PluginHost;
use test_case::test_case;

#[test_case("index", include_str!("../../plugins/index/tests/spec.lua") ; "index_plugin_spec")]
#[test_case("webfetch", include_str!("../../plugins/webfetch/tests/spec.lua") ; "webfetch_plugin_spec")]
fn plugin_spec(name: &str, spec: &str) {
    let config = PluginsConfig {
        enabled: true,
        builtins: vec![],
        init_file: None,
    };
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(&config, Arc::clone(&reg)).unwrap();
    host.load_source(&format!("{name}_spec"), spec)
        .unwrap_or_else(|e| panic!("{name} spec failed:\n{e}"));
}
