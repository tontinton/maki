use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_lua::{PluginHost, PluginPermissions};

const REQUEST_STAGE: &str = "request";
const RESPONSE_END_STAGE: &str = "response_end";
const HOOK_SUFFIX: &str = " [hooked]";
const STOPPED_VALUE: &str = "short";

fn host() -> PluginHost {
    PluginHost::new(Arc::new(ToolRegistry::new())).unwrap()
}

const REQUEST_HOOK_PLUGIN: &str = r#"
maki.provider.register_request_hook({
    callback = function(ctx)
        ctx.system = ctx.system .. " [hooked]"
        return ctx
    end,
})
"#;

const RESPONSE_HOOK_PLUGIN: &str = r#"
maki.provider.register_response_hook({
    callback = function(ctx)
        ctx.message = "transformed"
        return ctx
    end,
})
"#;

const SLUG_FILTER_PLUGIN: &str = r#"
maki.provider.register_request_hook({
    slug = "other",
    callback = function(ctx)
        ctx.system = ctx.system .. " [should-not-fire]"
        return ctx
    end,
})
"#;

const STOP_HOOK_PLUGIN: &str = r#"
maki.provider.register_request_hook({
    callback = function(ctx)
        return { stop = true, value = { system = "short" } }
    end,
})
maki.provider.register_request_hook({
    callback = function(ctx)
        ctx.system = ctx.system .. " [second]"
        return ctx
    end,
})
"#;

const ERROR_HOOK_PLUGIN: &str = r#"
maki.provider.register_request_hook({
    callback = function(ctx)
        error("boom")
    end,
})
maki.provider.register_request_hook({
    callback = function(ctx)
        ctx.system = ctx.system .. " [after-error]"
        return ctx
    end,
})
"#;

#[test]
fn request_hook_mutates_ctx() {
    let host = host();
    host.load_source_with_permissions("h", REQUEST_HOOK_PLUGIN, PluginPermissions::trusted())
        .unwrap();
    let handle = host.event_handle().expect("event handle");

    let ctx = serde_json::json!({ "system": "hi", "messages": [], "tools": [] });
    let out = smol::block_on(handle.run_provider_hooks(REQUEST_STAGE, "test", ctx)).unwrap();
    assert_eq!(out["system"].as_str().unwrap(), format!("hi{HOOK_SUFFIX}"));
}

#[test]
fn response_hook_mutates_message() {
    let host = host();
    host.load_source_with_permissions("h", RESPONSE_HOOK_PLUGIN, PluginPermissions::trusted())
        .unwrap();
    let handle = host.event_handle().expect("event handle");

    let ctx = serde_json::json!({ "message": { "role": "assistant", "content": "original" } });
    let out = smol::block_on(handle.run_provider_hooks(RESPONSE_END_STAGE, "test", ctx)).unwrap();
    assert_eq!(out["message"].as_str().unwrap(), "transformed");
}

#[test]
fn slug_filter_skips_non_matching() {
    let host = host();
    host.load_source_with_permissions("h", SLUG_FILTER_PLUGIN, PluginPermissions::trusted())
        .unwrap();
    let handle = host.event_handle().expect("event handle");

    let ctx = serde_json::json!({ "system": "hi" });
    let out = smol::block_on(handle.run_provider_hooks(REQUEST_STAGE, "test", ctx)).unwrap();
    assert_eq!(out["system"].as_str().unwrap(), "hi");
}

#[test]
fn stop_short_circuits_remaining_hooks() {
    let host = host();
    host.load_source_with_permissions("h", STOP_HOOK_PLUGIN, PluginPermissions::trusted())
        .unwrap();
    let handle = host.event_handle().expect("event handle");

    let ctx = serde_json::json!({ "system": "begin" });
    let out = smol::block_on(handle.run_provider_hooks(REQUEST_STAGE, "test", ctx)).unwrap();
    assert_eq!(out["system"].as_str().unwrap(), STOPPED_VALUE);
}

#[test]
fn failing_hook_skipped_but_chain_continues() {
    let host = host();
    host.load_source_with_permissions("h", ERROR_HOOK_PLUGIN, PluginPermissions::trusted())
        .unwrap();
    let handle = host.event_handle().expect("event handle");

    let ctx = serde_json::json!({ "system": "begin" });
    let out = smol::block_on(handle.run_provider_hooks(REQUEST_STAGE, "test", ctx)).unwrap();
    assert_eq!(out["system"].as_str().unwrap(), "begin [after-error]");
}

#[test]
fn no_hooks_returns_ctx_unchanged() {
    let host = host();
    let handle = host.event_handle().expect("event handle");

    let ctx = serde_json::json!({ "system": "untouched" });
    let out = smol::block_on(handle.run_provider_hooks(REQUEST_STAGE, "test", ctx)).unwrap();
    assert_eq!(out["system"].as_str().unwrap(), "untouched");
}
