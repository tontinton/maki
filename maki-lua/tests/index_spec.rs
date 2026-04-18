use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_config::PluginsConfig;
use maki_lua::PluginHost;

#[test]
fn index_plugin_spec() {
    let config = PluginsConfig {
        enabled: true,
        builtins: vec![],
        init_file: None,
    };
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(&config, Arc::clone(&reg)).unwrap();
    let spec = include_str!("../../plugins/index/tests/index_spec.lua");
    host.load_source("index_spec", spec)
        .unwrap_or_else(|e| panic!("index spec failed:\n{e}"));
}
