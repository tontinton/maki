use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use maki_agent::ToolOutput;
use maki_agent::template::Vars;
use maki_agent::tools::{
    DescriptionContext, ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolAudience,
    ToolContext, ToolExecResult, ToolFilter, ToolInvocation, ToolLive, ToolRegistry, ToolSource,
    timeout_annotation,
};
use maki_config::{
    AlwaysThinking, EDIT_SUB_TOOLS, Effect, FILE_WRITE_TOOLS, Permission, PluginsConfig, ToolKey,
    ToolOutputLines,
};
use maki_lua::{
    MAX_INFLIGHT_TOOLS, PERMISSION_NAME_WARNING, PluginError, PluginHost, SKIPPED_PLUGIN_WARNING,
    SessionEndReason, WARM_TOOL_CAP,
};
use maki_providers::Model;
use maki_storage::id::SessionRef;
#[cfg(unix)]
use rustix::process::{Pid, test_kill_process_group};
use serde_json::{Value, json};

const BUILTIN_COMMANDS: &[&str] = &["/sessions", "/rename", "/tasks"];
const NARGS_ERR: &str = r#"'nargs' must be 0, 1, "?", "*", or "+""#;
const GLOBAL_PACK_ONLY_ERR: &str = "only available in the global init.lua";
const USAGE_TOOL_NAME: &str = "usage_child";
const USAGE_VALUE: &str = "12.3k↑ 456↓ $0.123";
const USAGE_OUTPUT: &str = "usage_done";
const FLOORED_PACKAGE: &str = "future_pack";
const SIBLING_PACKAGE: &str = "sibling_pack";
const MALFORMED_FLOOR: &str = "min_maki_version = 12\n";
const SHADOWED_TOOL: &str = "skill";
const REPLACEMENT_PLUGIN: &str = "my_skill";
const REPLACEMENT_DESC: &str = "took the builtin name over";
const PERMISSION_KEYED_TOOL: &str = "task";
const OTHER_PERMISSION_KEYED_TOOL: &str = "write";
const PLAIN_TOOL: &str = "plain_helper";
const FILE_WRITE_TOOLS_DRIFT: &str = "fs_write tool declarations drifted from FILE_WRITE_TOOLS, update the const or the \
     register_tool declaration";
const MEMORY_RULES_DROPPED: &str = "memory pre-approved tools nobody had registered yet, so it must load after the plugins owning them";

/// Lua tools cannot publish `ToolLive::Usage` (only the subagent relay does), so
/// a native stub stands in for one.
struct UsageTool;

impl ToolInvocation for UsageTool {
    fn start_header(&self) -> HeaderFuture {
        HeaderFuture::Ready(HeaderResult::plain(USAGE_TOOL_NAME.into()))
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            if let Some(sink) = &ctx.live_sink {
                let _ = sink.send(ToolLive::Usage(USAGE_VALUE.into()));
            }
            ToolExecResult::from(Ok::<_, String>(ToolOutput::Plain(USAGE_OUTPUT.into())))
        })
    }
}

impl Tool for UsageTool {
    fn name(&self) -> &str {
        USAGE_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        "emits usage".into()
    }

    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    fn parse(&self, _input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        Ok(Box::new(UsageTool))
    }
}

fn fresh_registry() -> Arc<ToolRegistry> {
    Arc::new(ToolRegistry::new())
}

fn builtins_host() -> (Arc<ToolRegistry>, PluginHost) {
    builtins_host_with(&PluginsConfig::from_plugins(HashMap::new()))
}

fn builtins_host_with(config: &PluginsConfig) -> (Arc<ToolRegistry>, PluginHost) {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_builtins(config).unwrap();
    (reg, host)
}

fn exec_tool(reg: &ToolRegistry, name: &str, input: serde_json::Value) -> Result<String, String> {
    exec_tool_in(reg, name, input, None)
}

fn exec_tool_in(
    reg: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
    registry_override: Option<Arc<ToolRegistry>>,
) -> Result<String, String> {
    exec_output_in(reg, name, input, registry_override).map(|out| match out {
        maki_agent::ToolOutput::Plain(s) => s.text,
        other => panic!("unexpected output: {other:?}"),
    })
}

fn exec_tool_output(
    reg: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
) -> Result<maki_agent::ToolOutput, String> {
    exec_output_in(reg, name, input, None)
}

fn exec_output_in(
    reg: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
    registry_override: Option<Arc<ToolRegistry>>,
) -> Result<maki_agent::ToolOutput, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    let mut ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    if let Some(r) = registry_override {
        ctx.registry = r;
    }
    smol::block_on(async { inv.execute(&ctx).await }).output
}

const ECHO_PLUGIN: &str = r#"
maki.api.register_tool({
    name = "echo_",
    description = "echo",
    schema = {
        type = "object",
        properties = { msg = { type = "string" } },
        required = { "msg" }
    },
    audiences = { "main" },
    handler = function(input, ctx)
        return input.msg
    end
})
"#;

const MINIMAL_SCHEMA: &str =
    r#"{ type = "object", properties = {}, additionalProperties = false }"#;

const STRING_FIELD_SCHEMA: &str = r#"{
    type = "object",
    properties = { url = { type = "string" } },
    required = { "url" },
}"#;

const INVALID_PERMISSION_SCOPE_ERR: &str = "not in schema properties or not type 'string'";
const BAD_NAME_SRC: &str = r#"name = "bad name!", description = "test""#;
const EMPTY_DESC_SRC: &str = r#"name = "valid_name", description = """#;
const EMPTY_AUD_SRC: &str = r#"name = "no_aud", description = "test", audiences = {}"#;
const UNKNOWN_AUD_SRC: &str =
    r#"name = "bad_aud", description = "test", audiences = { "wurkflow" }"#;
const STRING_EXAMPLES_SRC: &str = r#"name = "ex_bad", description = "test", examples = "[]""#;
const TIMEOUT_FIELD_NOT_IN_SCHEMA_SRC: &str = r#"name = "to_bad", description = "test", start_annotation = { field = "timeout", kind = "timeout" }"#;
const SCOPE_MISSING_FIELD_SRC: &str = r#"name = "bad_scope", description = "test", permission = "fs_write", permission_scopes = "nonexistent""#;
const SCOPE_NON_STRING_FIELD_SRC: &str = r#"name = "bad_scope", description = "test", permission = "fs_write", permission_scopes = "count""#;
const OLD_SCOPE_KEY_SRC: &str =
    r#"name = "old_key", description = "test", permission_scope = "url""#;
const WRONG_TYPE_SCOPES_SRC: &str =
    r#"name = "num_scope", description = "test", permission = "fs_write", permission_scopes = 42"#;
const SCOPES_WITHOUT_PERMISSION_SRC: &str =
    r#"name = "no_perm", description = "test", permission_scopes = "url""#;
const PERMISSION_WITHOUT_SCOPES_SRC: &str =
    r#"name = "no_scopes", description = "test", permission = "fs_write""#;
const UNKNOWN_PERMISSION_SRC: &str = r#"name = "bad_perm", description = "test", permission = "filesystem", permission_scopes = "url""#;
const FS_WRITE_WITHOUT_MUTABLE_PATH_SRC: &str = r#"name = "no_mpath", description = "test", permission = "fs_write", permission_scopes = "url""#;
const NON_STRING_FIELD_SCHEMA: &str = r#"{
    type = "object",
    properties = { count = { type = "integer" } },
    required = { "count" },
}"#;

const CODE_SCHEMA: &str = r#"{
    type = "object",
    properties = { code = { type = "string" } },
    required = { "code" },
}"#;

const TIMEOUT_SCHEMA: &str = r#"{
    type = "object",
    properties = { timeout = { type = "integer" } },
    required = { "timeout" },
}"#;

const ARRAY_SCHEMA: &str = r#"{
    type = "object",
    properties = { edits = { type = "array", items = { type = "integer" } } },
    required = { "edits" },
}"#;

const START_ANNOTATION_COUNT_NON_ARRAY_SRC: &str =
    r#"name = "sa_bad", description = "test", start_annotation = "name""#;
const STRING_NAME_SCHEMA: &str = r#"{
    type = "object",
    properties = { name = { type = "string" } },
    required = { "name" },
}"#;
const JOB_BAD_CWD: &str = "~/definitely/not/a/dir";
const JOB_BAD_CWD_ERR_PREFIX: &str = "cwd is not a directory: ";
const NIL_WITHOUT_JOBS_ERR: &str =
    "handler returned nil without calling ctx:finish() or starting jobs";
const FINISH_CALLED_TWICE_ERR: &str = "ctx:finish() already called";
const DEADLINE_ALREADY_SET_ERR: &str = "ctx:set_deadline() already called";
const TIMED_OUT_SUBSTR: &str = "timed out";
const ALREADY_CALLED_ERR: &str = "already called";
const UNKNOWN_FIELD_ERR: &str = "unknown field";
const PERMISSION_DENIED_MSG: &str = "permission denied";

#[test]
fn stdlib_globals_accessible() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    for global in &["os", "debug", "string", "table", "math"] {
        let source =
            format!(r#"if {global} == nil then error("stdlib missing: {global} is nil") end"#);
        host.load_source(&format!("stdlib_check_{global}"), &source)
            .unwrap_or_else(|e| panic!("stdlib check for {global} failed: {e}"));
    }
}

#[test]
fn dangerous_globals_blocked() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    for global in &["io", "package"] {
        let source =
            format!(r#"if {global} ~= nil then error("sandbox leak: {global} is not nil") end"#);
        host.load_source(&format!("sandbox_check_{global}"), &source)
            .unwrap_or_else(|e| panic!("sandbox check for {global} failed: {e}"));
    }
}

#[test]
fn register_echo_tool() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("echo_plugin", ECHO_PLUGIN).unwrap();

    let entry = reg.get("echo_").expect("echo_ tool not registered");
    assert_eq!(entry.tool.name(), "echo_");
    assert!(
        matches!(entry.source, ToolSource::Lua { ref plugin } if plugin.as_ref() == "echo_plugin"),
    );
    assert_eq!(entry.tool.tool_kind(), None);

    let out = exec_tool(&reg, "echo_", serde_json::json!({"msg": "hello"})).unwrap();
    assert_eq!(out, "hello");
}

const SESSION_PLUGIN: &str = r#"
maki.api.register_tool({
    name = "whoami",
    description = "reports the calling session",
    schema = { type = "object", properties = {}, additionalProperties = false },
    handler = function(_, ctx)
        local id, err = ctx:session_id()
        if err then
            return "err:" .. err
        end
        return "id:" .. tostring(id)
    end,
})
"#;

fn exec_with_ctx(
    reg: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
    ctx: &ToolContext,
) -> Result<String, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    smol::block_on(async { inv.execute(ctx).await })
        .output
        .map(|out| match out {
            maki_agent::ToolOutput::Plain(s) => s.text,
            other => panic!("unexpected output: {other:?}"),
        })
}

/// The point of the whole thing: a handler learns who called it without
/// asking `maki.session.current()`, which answers with whoever is focused.
#[test]
fn handler_reads_the_calling_session() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("session_plugin", SESSION_PLUGIN).unwrap();

    let session: SessionRef = "01965087-4c71-7f00-8000-000000000000"
        .parse()
        .expect("valid session id");
    let mut ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    ctx.session_id = Some(session.clone());

    let out = exec_with_ctx(&reg, "whoami", json!({}), &ctx).unwrap();
    assert_eq!(
        out,
        format!("id:{}", session.id()),
        "lua sees the canonical form, so it compares equal to maki.session.current()"
    );
    assert_ne!(
        out,
        format!("id:{}", session.as_str()),
        "the verbatim form would not match ids from maki.session.live()"
    );
}

#[test]
fn handler_without_a_session_gets_nil_and_no_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("session_plugin", SESSION_PLUGIN).unwrap();

    let ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    assert_eq!(
        exec_with_ctx(&reg, "whoami", json!({}), &ctx).unwrap(),
        "id:nil"
    );
}

#[test]
fn unload_round_trip() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    host.load_source("unload_test", ECHO_PLUGIN).unwrap();
    assert!(reg.has("echo_"));

    host.unload("unload_test").unwrap();
    assert!(!reg.has("echo_"));
}

const COMPLETER_SRC: &str = r#"
maki.api.register_input_completer({
  trigger = "@",
  name = "files",
  handler = function(query)
    return {
      { label = "src/" .. query, detail = "recent" },
      { label = "other", insert = "OTHER" },
    }
  end,
})"#;

#[test]
fn input_completer_publishes_answers_and_unloads() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    host.load_source("mention", COMPLETER_SRC).unwrap();
    let snap = host.completer_reader().load_full();
    assert_eq!(snap.completers.len(), 1);
    assert_eq!(snap.completers[0].trigger, '@');
    assert_eq!(&*snap.completers[0].plugin, "mention");
    assert_eq!(&*snap.completers[0].name, "files");

    let items = host
        .event_handle()
        .query_input_completer(Arc::from("mention"), Arc::from("files"), "ma".into())
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("handler should answer");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].label, "src/ma");
    assert_eq!(items[0].insert, "src/ma", "insert defaults to label");
    assert_eq!(items[0].detail.as_deref(), Some("recent"));
    assert_eq!(items[1].insert, "OTHER");

    host.unload("mention").unwrap();
    assert!(host.completer_reader().load_full().completers.is_empty());
}

#[test]
fn unregister_input_completer_removes_one_and_republishes() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "mentions",
        r##"
        maki.api.register_input_completer({ trigger = "@", name = "files", handler = function() end })
        maki.api.register_input_completer({ trigger = "#", name = "prs", handler = function() end })
        maki.api.unregister_input_completer("files")
        maki.api.unregister_input_completer("never-existed")
        "##,
    )
    .unwrap();

    let snap = host.completer_reader().load_full();
    assert_eq!(snap.completers.len(), 1);
    assert_eq!(&*snap.completers[0].name, "prs");
}

#[test]
fn input_completer_rejects_duplicate_trigger_across_plugins() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "first",
        r#"maki.api.register_input_completer({ trigger = "@", name = "files", handler = function() end })"#,
    )
    .unwrap();
    let err = host
        .load_source(
            "second",
            r#"maki.api.register_input_completer({ trigger = "@", name = "files", handler = function() end })"#,
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("already registered by plugin 'first'"),
        "unexpected error: {err}"
    );
    host.load_source(
        "first",
        r#"maki.api.register_input_completer({ trigger = "@", name = "files", handler = function() return {} end })"#,
    )
    .expect("same plugin may replace its own registration");
}

#[test]
fn input_completer_malformed_items_answer_none() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "mal",
        r##"maki.api.register_input_completer({
            trigger = "@",
            name = "missing_label",
            handler = function() return { { insert = "x" } } end,
        })
        maki.api.register_input_completer({
            trigger = "#",
            name = "wrong_type",
            handler = function() return { { label = "ok", insert = 42 } } end,
        })"##,
    )
    .unwrap();

    for name in ["missing_label", "wrong_type"] {
        let out = host
            .event_handle()
            .query_input_completer(Arc::from("mal"), Arc::from(name), String::new())
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(out.is_none(), "{name} should error, not fall back");
    }
}

#[test]
fn input_completer_unknown_name_and_handler_error_answer_none() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "broken",
        r##"maki.api.register_input_completer({
            trigger = "#",
            name = "boom",
            handler = function() error("nope") end,
        })"##,
    )
    .unwrap();

    let unknown = host
        .event_handle()
        .query_input_completer(Arc::from("broken"), Arc::from("missing"), String::new())
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(unknown.is_none());

    let failed = host
        .event_handle()
        .query_input_completer(Arc::from("broken"), Arc::from("boom"), String::new())
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(failed.is_none());
}

#[test]
fn input_completer_rejects_bad_specs() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let cases = [
        r#"maki.api.register_input_completer({ trigger = "ab", name = "x", handler = function() end })"#,
        r#"maki.api.register_input_completer({ trigger = "a", name = "x", handler = function() end })"#,
        r#"maki.api.register_input_completer({ trigger = " ", name = "x", handler = function() end })"#,
        r#"maki.api.register_input_completer({ trigger = "@", name = "", handler = function() end })"#,
        r#"maki.api.register_input_completer({ trigger = "@", name = "x" })"#,
        r#"maki.api.register_input_completer({ trigger = "_", name = "x", handler = function() end })"#,
    ];
    for (i, src) in cases.iter().enumerate() {
        assert!(
            host.load_source(&format!("bad_completer_{i}"), src)
                .is_err(),
            "case {i} should be rejected"
        );
    }
}

const PERMISSION_RULE_SRC: &str =
    r#"maki.api.register_permission_rule({ tool = "edit", scope = "/tmp/x/**" })"#;
const NO_RULE_SRC: &str = "local _ = 1";
/// A rule can only name a registered tool, and it reads the permission it needs
/// off that tool, so the rule tests have to provide one.
const EDIT_TOOL_SRC: &str = r#"maki.api.register_tool({
    name = "edit",
    description = "test edit tool",
    schema = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
    permission = "fs_write",
    permission_scopes = "path",
    mutable_path = "path",
    handler = function() return "" end,
})"#;

#[test]
fn permission_rule_lands_in_store_and_unload_clears() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    host.load_source("tool_owner", EDIT_TOOL_SRC).unwrap();
    host.load_source("perm_plugin", PERMISSION_RULE_SRC)
        .unwrap();
    let rules = host.plugin_rules().snapshot();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].tool, ToolKey::native("edit"));
    assert_eq!(rules[0].scope.as_deref(), Some("/tmp/x/**"));
    assert_eq!(rules[0].effect, Effect::Allow);

    host.unload("perm_plugin").unwrap();
    assert!(host.plugin_rules().snapshot().is_empty());
}

#[test]
fn permission_rule_failed_load_leaves_store_empty() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    host.load_source("tool_owner", EDIT_TOOL_SRC).unwrap();
    let src = format!("{PERMISSION_RULE_SRC}\nerror('boom after rule')");
    let err = host
        .load_source("perm_broken", &src)
        .expect_err("expected lua error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(host.plugin_rules().snapshot().is_empty());
}

#[test]
fn reload_clears_stale_rules_of_that_plugin_only() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    host.load_source("tool_owner", EDIT_TOOL_SRC).unwrap();
    host.load_source("perm_a", PERMISSION_RULE_SRC).unwrap();
    host.load_source(
        "perm_b",
        r#"maki.api.register_permission_rule({ tool = "write", scope = "/tmp/y/**", effect = "deny" })"#,
    )
    .unwrap();
    assert_eq!(host.plugin_rules().snapshot().len(), 2);

    host.load_source("perm_a", NO_RULE_SRC).unwrap();
    let rules = host.plugin_rules().snapshot();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].tool, ToolKey::native("write"));
    assert_eq!(rules[0].scope.as_deref(), Some("/tmp/y/**"));
    assert_eq!(rules[0].effect, Effect::Deny);
}

const TOOL_PERMISSION_NOT_GRANTED: &str = "which this plugin was not granted";
/// No `permission_scopes`, so the permission manager never consults it and a
/// rule naming it could only ever do nothing.
const UNCHECKED_EDIT_TOOL_SRC: &str = r#"maki.api.register_tool({
    name = "edit",
    description = "unchecked",
    schema = { type = "object", properties = {} },
    handler = function() return "" end,
})"#;

/// A package is the only entry point that runs lua under a permission set the
/// plugin did not pick for itself.
fn load_package_with(
    host: &PluginHost,
    src: &str,
    permissions: maki_lua::PluginPermissions,
) -> Result<(), PluginError> {
    let pkg = package_dir(&[("plugin.lua", src)]);
    host.load_package("pack", pkg.path(), permissions, Default::default())
}

/// An allow is delegation, so it survives only when the plugin holds the
/// permission the named tool exposes and that tool is one the permission
/// manager would ever consult. When it does not survive it costs the rule and
/// not the plugin: the call simply prompts as it would have without it.
#[test_case::test_case(EDIT_TOOL_SRC, true => 1 ; "granted_plugin_pre_approves_a_checked_tool")]
#[test_case::test_case(EDIT_TOOL_SRC, false => 0 ; "plugin_granted_nothing_pre_approves_nothing")]
#[test_case::test_case(NO_RULE_SRC, true => 0 ; "no_such_tool_is_registered")]
#[test_case::test_case(UNCHECKED_EDIT_TOOL_SRC, true => 0 ; "tool_is_never_permission_checked")]
fn allow_rule_survives_only_when_delegated(owner_src: &str, granted: bool) -> usize {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("tool_owner", owner_src).unwrap();

    let permissions = if granted {
        maki_lua::PluginPermissions::trusted()
    } else {
        maki_lua::PluginPermissions::denied()
    };
    load_package_with(&host, PERMISSION_RULE_SRC, permissions)
        .expect("a rule that does not hold up must not fail the load");
    host.plugin_rules().snapshot().len()
}

/// A deny only ever takes authority away, so nothing about it is checked: not
/// the permission, not the blanket scope, not even whether the tool exists.
#[test]
fn deny_rule_is_never_filtered() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    load_package_with(
        &host,
        r#"maki.api.register_permission_rule({ tool = "edit", scope = "*", effect = "deny" })"#,
        maki_lua::PluginPermissions::denied(),
    )
    .unwrap();

    let rules = host.plugin_rules().snapshot();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].effect, Effect::Deny);
}

/// The delegation rule from the other side: shipping a tool is itself a use of
/// the permission that tool exposes.
#[test]
fn register_tool_cannot_expose_a_permission_it_lacks() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let err = load_package_with(&host, EDIT_TOOL_SRC, maki_lua::PluginPermissions::denied())
        .expect_err("a package granted nothing must not ship an fs_write tool");
    assert!(
        err.to_string().contains(TOOL_PERMISSION_NOT_GRANTED),
        "got: {err}"
    );
}

/// Rules resolve when the load commits, not while the chunks run, so a plugin
/// can pre-approve a tool it ships itself whichever line comes first.
#[test]
fn permission_rule_can_name_a_tool_the_same_plugin_registers() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    host.load_source(
        "self_owner",
        &format!("{PERMISSION_RULE_SRC}\n{EDIT_TOOL_SRC}"),
    )
    .unwrap();

    assert_eq!(host.plugin_rules().snapshot().len(), 1);
}

#[test_case::test_case(r#"{ tool = "srv.tool", scope = "/x/**" }"#, "only native tools are allowed" ; "mcp_tool")]
#[test_case::test_case(r#"{ tool = "mcp:srv", scope = "/x/**" }"#, "invalid tool name" ; "invalid_tool_chars")]
#[test_case::test_case(r#"{ tool = "*", scope = "/x/**" }"#, "only native tools are allowed" ; "wildcard_tool")]
#[test_case::test_case(r#"{ scope = "/x/**" }"#, "'tool' must be a native tool name string" ; "missing_tool")]
#[test_case::test_case(r#"{ tool = "edit" }"#, "'scope' must be a string" ; "missing_scope")]
#[test_case::test_case(r#"{ tool = "edit", scope = "" }"#, "'scope' must be non-empty" ; "empty_scope")]
#[test_case::test_case(r#"{ tool = "edit", scope = "/x/**", effect = "maybe" }"#, "invalid effect 'maybe'" ; "bad_effect")]
#[test_case::test_case(r#"{ tool = "edit", scope = "/x/**", bogus = 1 }"#, "unknown key 'bogus'" ; "unknown_key")]
#[test_case::test_case(r#"{ tool = "edit", scope = "*" }"#, "matches every scope" ; "star_scope")]
#[test_case::test_case(r#"{ tool = "edit", scope = "**" }"#, "matches every scope" ; "double_star_scope")]
#[test_case::test_case(r#"{ tool = "edit", scope = "/*" }"#, "matches every scope" ; "root_star_scope")]
#[test_case::test_case(r#"{ tool = "edit", scope = "/**" }"#, "matches every scope" ; "root_double_star_scope")]
fn permission_rule_validation_rejects(spec: &str, expected_err: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_source(
            "perm_invalid",
            &format!("maki.api.register_permission_rule({spec})"),
        )
        .expect_err("expected validation error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains(expected_err), "got: {err}");
}

#[test_case::test_case(BAD_NAME_SRC, MINIMAL_SCHEMA, "invalid name" ; "invalid_tool_name")]
#[test_case::test_case(EMPTY_DESC_SRC, MINIMAL_SCHEMA, "description must be non-empty" ; "empty_description")]
#[test_case::test_case(EMPTY_AUD_SRC, MINIMAL_SCHEMA, "audiences" ; "empty_audiences")]
#[test_case::test_case(UNKNOWN_AUD_SRC, MINIMAL_SCHEMA, "unknown audience" ; "unknown_audience")]
#[test_case::test_case(STRING_EXAMPLES_SRC, MINIMAL_SCHEMA, "'examples' must be a table" ; "string_examples")]
#[test_case::test_case(TIMEOUT_FIELD_NOT_IN_SCHEMA_SRC, MINIMAL_SCHEMA, "not type 'integer'" ; "timeout_field_not_in_schema")]
#[test_case::test_case(SCOPE_MISSING_FIELD_SRC, STRING_FIELD_SCHEMA, INVALID_PERMISSION_SCOPE_ERR ; "permission_scopes_missing_field")]
#[test_case::test_case(SCOPE_NON_STRING_FIELD_SRC, NON_STRING_FIELD_SCHEMA, INVALID_PERMISSION_SCOPE_ERR ; "permission_scopes_non_string_field")]
#[test_case::test_case(OLD_SCOPE_KEY_SRC, MINIMAL_SCHEMA, "'permission_scope' was removed" ; "old_permission_scope_key")]
#[test_case::test_case(WRONG_TYPE_SCOPES_SRC, MINIMAL_SCHEMA, "'permission_scopes' must be a string field name or a function" ; "permission_scopes_wrong_type")]
#[test_case::test_case(SCOPES_WITHOUT_PERMISSION_SRC, STRING_FIELD_SCHEMA, "must declare 'permission'" ; "scopes_without_permission")]
#[test_case::test_case(PERMISSION_WITHOUT_SCOPES_SRC, STRING_FIELD_SCHEMA, "needs 'permission_scopes'" ; "permission_without_scopes")]
#[test_case::test_case(UNKNOWN_PERMISSION_SRC, STRING_FIELD_SCHEMA, "unknown permission 'filesystem'" ; "unknown_permission")]
#[test_case::test_case(FS_WRITE_WITHOUT_MUTABLE_PATH_SRC, STRING_FIELD_SCHEMA, "no 'mutable_path'" ; "fs_write_without_mutable_path")]
fn registration_validation_rejects(fields: &str, schema: &str, expected_err: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            {fields},
            schema = {schema},
            handler = function(input, ctx) return "" end
        }})"#,
    );
    let err = host
        .load_source("validation_test", &src)
        .expect_err("expected validation error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains(expected_err), "got: {err}");
}

#[test]
fn permission_scopes_valid_string_field_accepted() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "ok_scope",
            description = "test",
            schema = {STRING_FIELD_SCHEMA},
            permission = "net",
            permission_scopes = "url",
            handler = function() return "" end
        }})"#,
    );
    host.load_source("ok_scope_plugin", &src).unwrap();
    assert!(reg.has("ok_scope"));
}

#[test]
fn tool_kind_flows_to_trait() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "my_fetcher",
            description = "fetches things",
            schema = {MINIMAL_SCHEMA},
            kind = "fetch",
            handler = function() return "" end
        }})"#,
    );
    host.load_source("kind_plugin", &src).unwrap();
    let entry = reg.get("my_fetcher").expect("tool not registered");
    assert_eq!(entry.tool.tool_kind(), Some("fetch"));
}

/// `get_tool` handles are the boundary between plugins: they never throw
/// (errors become nil) and their returns are normalized, so a composing
/// caller like batch needs no pcall of its own.
#[test]
fn get_tool_returns_normalized_header_and_restore_handles() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"
        maki.api.register_tool({{
            name = "styled_tool",
            description = "t",
            schema = {STRING_FIELD_SCHEMA},
            handler = function() return "ok" end,
            header = function(input) return "H:" .. input.url end,
            restore = function(input)
                if input.with_body then
                    local b = maki.ui.buf()
                    b:line("body")
                    return {{ body = b }}
                end
                return {{}}
            end,
        }})
        maki.api.register_tool({{
            name = "throwing_tool",
            description = "t",
            schema = {MINIMAL_SCHEMA},
            handler = function() return "ok" end,
            header = function() error("kaboom") end,
            restore = function() error("kaboom") end,
        }})
        maki.api.register_tool({{
            name = "handle_probe",
            description = "p",
            schema = {MINIMAL_SCHEMA},
            handler = function()
                local t = maki.api.get_tool("styled_tool")
                if not t then return nil, "not found" end
                local thrower = maki.api.get_tool("throwing_tool")
                local h = t.header({{ url = "abc" }})
                return table.concat({{
                    t.name,
                    h[1][1] .. "/" .. h[1][2],
                    type(t.restore({{}}, "", false, nil)),
                    type(t.restore({{ with_body = true }}, "", false, nil)),
                    tostring(thrower.header({{}}) == nil),
                    tostring(thrower.restore({{}}, "", false, nil) == nil),
                    tostring(maki.api.get_tool("nope_tool") == nil),
                    type(maki.api.get_tool("handle_probe").header),
                }}, "|")
            end
        }})
        "#,
    );
    host.load_source("get_tool_plugin", &src).unwrap();

    let out = exec_tool(&reg, "handle_probe", serde_json::json!({})).unwrap();
    assert_eq!(
        out,
        "styled_tool|H:abc/tool|nil|userdata|true|true|true|nil"
    );
}

#[test]
fn handler_state_flows_to_tool_output_and_serde() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "stateful",
            description = "t",
            schema = {MINIMAL_SCHEMA},
            handler = function()
                return {{ llm_output = "done", state = {{ n = 3, tag = "hi" }} }}
            end
        }})"#,
    );
    host.load_source("state_plugin", &src).unwrap();

    let entry = reg.get("stateful").unwrap();
    let inv = entry.tool.parse(&serde_json::json!({})).unwrap();
    let ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    let out = smol::block_on(async { inv.execute(&ctx).await })
        .output
        .unwrap();
    let expected = serde_json::json!({ "n": 3, "tag": "hi" });
    assert_eq!(out.state(), Some(&expected));

    let json = serde_json::to_string(&out).unwrap();
    let parsed: maki_agent::ToolOutput = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.state(), Some(&expected), "state must survive serde");
}

/// Restores `tool` from `src` and returns the snapshot's concatenated text.
fn restore_snapshot_text(
    src: &str,
    tool: &str,
    clicks: Vec<usize>,
    state: Option<serde_json::Value>,
) -> String {
    let host = PluginHost::new(fresh_registry()).unwrap();
    host.load_source("restore_plugin", src).unwrap();
    let handle = host.event_handle();
    let (tx, rx) = flume::unbounded();

    handle.request_restore(
        maki_lua::RestoreItem {
            tool: Arc::from(tool),
            tool_use_id: "restore_id".to_owned(),
            output: "ok".to_owned(),
            input: serde_json::json!({}),
            is_error: false,
            tool_output_lines: ToolOutputLines::default(),
            theme_gen: None,
            clicks,
            state,
        },
        maki_agent::EventSender::new(tx, 0),
    );
    handle.wait_restore_complete_for_test();

    let mut text = String::new();
    for env in rx.drain() {
        if let maki_agent::AgentEvent::ToolSnapshot { snapshot, .. } = env.event {
            for line in snapshot.lines.iter() {
                for span in &line.spans {
                    text.push_str(&span.text);
                }
            }
        }
    }
    text
}

#[test_case::test_case(true, "n=3 tag=hi" ; "state_present")]
#[test_case::test_case(false, "no state" ; "state_absent_falls_back")]
fn restore_reads_persisted_state(with_state: bool, expected: &str) {
    let state = with_state.then(|| serde_json::json!({ "n": 3, "tag": "hi" }));
    let src = format!(
        r#"maki.api.register_tool({{
            name = "state_restore",
            description = "t",
            schema = {MINIMAL_SCHEMA},
            handler = function() return "ok" end,
            restore = function(input, output, is_error, rctx)
                local buf = maki.ui.buf()
                local s = rctx:state()
                if s == nil then
                    buf:line("no state")
                else
                    buf:line("n=" .. tostring(s.n) .. " tag=" .. s.tag)
                end
                return buf
            end
        }})"#,
    );
    let text = restore_snapshot_text(&src, "state_restore", Vec::new(), state);
    assert!(text.contains(expected), "expected {expected:?} in: {text}");
}

#[test]
fn restore_ctx_is_userdata_with_gated_capabilities() {
    let src = format!(
        r#"maki.api.register_tool({{
            name = "ctx_restore",
            description = "t",
            schema = {MINIMAL_SCHEMA},
            handler = function() return "ok" end,
            restore = function(input, output, is_error, rctx)
                local cfg, cfg_err = rctx:config()
                local _, fin_err = rctx:finish("x")
                local _, dl_err = rctx:set_deadline(5)
                local parts = {{
                    rctx:state().tag,
                    type(rctx:tool_output_lines()) == "table" and "tol_ok" or "tol_bad",
                    (cfg == nil and cfg_err ~= nil) and "config_err" or "config_ok",
                    fin_err ~= nil and "finish_err" or "finish_ok",
                    dl_err ~= nil and "deadline_err" or "deadline_ok",
                    rctx:cancelled() == false and "cancelled_ok" or "cancelled_bad",
                }}
                local buf = maki.ui.buf()
                buf:line(table.concat(parts, " "))
                return buf
            end
        }})"#
    );
    let text = restore_snapshot_text(
        &src,
        "ctx_restore",
        Vec::new(),
        Some(serde_json::json!({ "tag": "hi" })),
    );
    assert!(
        text.contains("hi tol_ok config_err finish_err deadline_err cancelled_ok"),
        "restore ctx capability matrix mismatch: {text}"
    );
}

#[test]
fn get_tool_restore_accepts_table_or_userdata_ctx() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"local probe
maki.api.register_tool({{
    name = "child_r",
    description = "t",
    schema = {MINIMAL_SCHEMA},
    handler = function() return "ok" end,
    restore = function(input, output, is_error, rctx)
        probe = {{ state = rctx:state(), tol = rctx:tool_output_lines() }}
        local buf = maki.ui.buf()
        buf:line("body")
        return buf
    end
}})
maki.api.register_tool({{
    name = "restore_driver",
    description = "t",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx)
        local t = maki.api.get_tool("child_r")
        local parts = {{}}
        local buf = t.restore({{}}, "out", false, {{ tool_output_lines = {{ bash = 42 }}, state = {{ tag = "T" }} }})
        parts[1] = buf ~= nil and "buf_ok" or "buf_nil"
        parts[2] = (probe.state and probe.state.tag == "T") and "state_ok" or "state_bad"
        parts[3] = probe.tol.bash == 42 and "tol_ok" or "tol_bad"
        probe = nil
        local buf2 = t.restore({{}}, "out", false, ctx)
        parts[4] = buf2 ~= nil and "buf2_ok" or "buf2_nil"
        parts[5] = (probe.state == nil and type(probe.tol) == "table") and "ud_ok" or "ud_bad"
        probe = nil
        local buf3 = t.restore({{}}, "out", false)
        parts[6] = (buf3 ~= nil and type(probe.tol) == "table") and "default_ok" or "default_bad"
        return table.concat(parts, " ")
    end
}})"#
    );
    host.load_source("restore_compose_plugin", &src).unwrap();
    let out = exec_tool(&reg, "restore_driver", serde_json::json!({})).unwrap();
    assert_eq!(
        out, "buf_ok state_ok tol_ok buf2_ok ud_ok default_ok",
        "wrap_restore ctx normalization mismatch"
    );
}

#[test]
fn agent_api_value_failures_return_err_pairs() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "agent_pairs_probe",
            description = "t",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local function pair_err(v, e)
                    return v == nil and type(e) == "string"
                end
                local parts = {{}}
                parts[1] = pair_err(maki.agent.system_prompt(ctx, {{ prompt_id = "nope" }})) and "prompt_err" or "prompt_ok"
                parts[2] = pair_err(maki.agent.tools(ctx, {{ audience = "nope" }})) and "tools_err" or "tools_ok"
                parts[3] = pair_err(maki.agent.resolve_model(ctx, {{ spec = "not-a-spec" }})) and "model_err" or "model_ok"
                return table.concat(parts, " ")
            end
        }})"#
    );
    host.load_source("agent_pairs_plugin", &src).unwrap();
    let out = exec_tool(&reg, "agent_pairs_probe", serde_json::json!({})).unwrap();
    assert_eq!(out, "prompt_err tools_err model_err");
}

/// `spec` must win over `tier` when both are given, proving the task plugin's
/// `model` field (forwarded as `spec`) takes precedence over `model_tier`.
#[test]
fn resolve_model_spec_takes_precedence_over_tier() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "spec_precedence_probe",
            description = "t",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local m, err = maki.agent.resolve_model(ctx, {{
                    tier = "weak",
                    spec = "anthropic/claude-opus-4-8",
                }})
                if err then return "err:" .. err end
                return m.spec
            end
        }})"#
    );
    host.load_source("spec_precedence_probe", &src).unwrap();
    let out = exec_tool(&reg, "spec_precedence_probe", serde_json::json!({})).unwrap();
    assert_eq!(
        out, "anthropic/claude-opus-4-8",
        "spec must override tier when both are set"
    );
}

/// Restore used to lose anything drawn via `maki.async.run`: those tasks
/// landed in the global spawn queue, which runs after the snapshot is
/// taken. The runtime must run them inline, after the restore fn and after
/// each replayed click.
#[test_case::test_case(Vec::new(), "restore async line" ; "restore_async_task_runs_inline")]
#[test_case::test_case(vec![0], "click async line" ; "click_replay_async_task_runs_inline")]
fn restore_snapshot_contains_async_run_content(clicks: Vec<usize>, expected: &str) {
    let src = format!(
        r#"maki.api.register_tool({{
            name = "async_restore",
            description = "t",
            schema = {MINIMAL_SCHEMA},
            handler = function() return "ok" end,
            restore = function(input, output, is_error, rctx)
                local buf = maki.ui.buf()
                buf:line("sync line")
                maki.async.run(function()
                    buf:line("restore async line")
                end)
                buf:on("click", function()
                    maki.async.run(function()
                        buf:line("click async line")
                    end)
                end)
                return buf
            end
        }})"#,
    );
    let text = restore_snapshot_text(&src, "async_restore", clicks, None);
    assert!(text.contains("sync line"), "sync content missing: {text}");
    assert!(
        text.contains(expected),
        "async content missing {expected:?}: {text}"
    );
}

#[test]
fn examples_table_flows_to_trait() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "with_examples",
            description = "test",
            schema = {STRING_FIELD_SCHEMA},
            examples = {{ {{ url = "https://example.com" }} }},
            handler = function() return "" end
        }})"#,
    );
    host.load_source("examples_plugin", &src).unwrap();
    let entry = reg.get("with_examples").expect("tool not registered");
    assert_eq!(
        entry.tool.examples(),
        Some(serde_json::json!([{"url": "https://example.com"}]))
    );
}

#[test]
fn interrupt_kills_infinite_loop_and_vm_recovers() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"
maki.api.register_tool({{
    name = "infinite_loop_",
    description = "loops forever",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx) while true do end end
}})
maki.api.register_tool({{
    name = "noop_after_loop",
    description = "returns ok",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx) return "ok" end
}})
"#,
    );
    host.load_source("loop_plugin", &src).unwrap();

    let entry = reg.get("infinite_loop_").expect("loop tool not registered");
    let inv = entry.tool.parse(&serde_json::json!({})).unwrap();
    let mut ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    ctx.deadline = maki_agent::tools::Deadline::after(std::time::Duration::from_secs(5));

    let result = smol::block_on(async { inv.execute(&ctx).await });

    assert!(result.output.is_err(), "expected error from timed-out loop");

    let ok = exec_tool(&reg, "noop_after_loop", serde_json::json!({}));
    assert!(ok.is_ok(), "VM poisoned after interrupt: {ok:?}");
}

#[test]
fn failed_load_leaves_no_tools_or_commands() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"
maki.api.register_tool({{
    name = "doomed",
    description = "never registered",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function() return "" end
}})
maki.api.register_command({{
    name = "/doomed",
    handler = function() end,
}})
error("plugin blew up after register")
"#,
    );
    let err = host
        .load_source("broken", &src)
        .expect_err("expected lua error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(!reg.has("doomed"));
    assert_eq!(host.command_reader().load().commands.len(), 0);

    host.load_source("broken", ECHO_PLUGIN)
        .expect("retry with good source should succeed");
    assert!(reg.has("echo_"));
}

#[test]
fn is_error_propagated_as_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "returns_error",
            description = "returns is_error=true",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                return {{ llm_output = "boom", is_error = true }}
            end
        }})"#,
    );
    host.load_source("err_plugin", &src).unwrap();

    let err = exec_tool(&reg, "returns_error", serde_json::json!({})).unwrap_err();
    assert_eq!(err, "boom");
}

#[test]
fn handler_bad_return_type_is_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "bad_ret_num",
            description = "bad return",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function() return 42 end
        }})"#,
    );
    host.load_source("bad_ret", &src).unwrap();

    let err = exec_tool(&reg, "bad_ret_num", serde_json::json!({})).unwrap_err();
    assert!(err.contains("must return string"), "got: {err}");
}

#[test]
fn handler_nil_without_jobs_is_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = r#"maki.api.register_tool({
        name = "nil_no_jobs",
        description = "returns nil without starting jobs",
        schema = { type = "object", properties = {} },
        audiences = { "main" },
        handler = function() return nil end
    })"#;
    host.load_source("nil_no_jobs", src).unwrap();
    let err = exec_tool(&reg, "nil_no_jobs", serde_json::json!({})).unwrap_err();
    assert!(err.contains(NIL_WITHOUT_JOBS_ERR), "got: {err}");
}

#[test]
fn handler_lua_error_surfaces_as_tool_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "thrower",
            description = "throws on call",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function() error("intentional kaboom") end
        }})"#,
    );
    host.load_source("thrower_plugin", &src).unwrap();

    let err = exec_tool(&reg, "thrower", serde_json::json!({})).unwrap_err();
    assert!(err.contains("intentional kaboom"), "got: {err}");
}

#[test]
fn lua_tool_schema_rejects_bad_input() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = r#"
maki.api.register_tool({
    name = "needs_name",
    description = "requires a name field",
    schema = {
        type = "object",
        properties = { name = { type = "string" } },
        required = { "name" }
    },
    handler = function(input) return input.name end
})
"#;
    host.load_source("schema_test", src).unwrap();

    let entry = reg.get("needs_name").unwrap();
    let err = entry
        .tool
        .parse(&serde_json::json!({"count": 1}))
        .err()
        .expect("missing required field should fail");
    assert!(err.to_string().contains("name"));

    assert!(
        entry
            .tool
            .parse(&serde_json::json!({"name": "alice"}))
            .is_ok()
    );
}

#[test]
fn init_lua_with_require_registers_tools() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(lua_dir.join("tools")).unwrap();

    std::fs::write(
        lua_dir.join("tools/greet.lua"),
        r#"
local M = {}
function M.setup()
    maki.api.register_tool({
        name = "greet",
        description = "says hi",
        schema = { type = "object", properties = {}, additionalProperties = false },
        handler = function() return "hi" end
    })
end
return M
"#,
    )
    .unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
local greet = require("tools.greet")
greet.setup()
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_plugin_file(&init_path).unwrap();

    assert!(reg.has("greet"));
    assert_eq!(reg.names().len(), 1);
}

/// An incompatible `plugin.toml` must cost that directory its Lua, not the
/// whole startup: `load_init_files` keeps going and reports a warning.
#[test]
fn incompatible_plugin_warns_instead_of_aborting_startup() {
    let tmp = tempfile::TempDir::new().unwrap();
    let maki_dir = tmp.path().join(".maki");
    std::fs::create_dir_all(&maki_dir).unwrap();
    let running = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
    let required = format!("{}.0.0", running.major + 1);
    std::fs::write(
        maki_dir.join("plugin.toml"),
        format!("min_maki_version = {required:?}\n"),
    )
    .unwrap();
    std::fs::write(maki_dir.join("init.lua"), ECHO_PLUGIN).unwrap();

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let mut warnings = Vec::new();
    host.load_init_files_or_skip(false, tmp.path(), &mut warnings)
        .expect("an incompatible plugin must not abort startup");

    assert!(!reg.has("echo_"));
    let warning = warnings
        .iter()
        .find(|w| w.contains(SKIPPED_PLUGIN_WARNING))
        .unwrap_or_else(|| panic!("no skip warning in {warnings:?}"));
    assert!(warning.contains(&required), "{warning}");
}

/// An `init.lua` registers tools on the same name-keyed permission model a
/// package does, and it is the path a plugin under the lua directory is loaded
/// from, so a name carrying maki's builtin defaults has to be reported here too.
#[test_case::test_case(PERMISSION_KEYED_TOOL, 1 ; "permission_keyed_name_warns")]
#[test_case::test_case(PLAIN_TOOL, 0 ; "ordinary_name_is_quiet")]
fn init_file_taking_a_permission_keyed_tool_name_warns(tool: &str, expected: usize) {
    let tmp = tempfile::TempDir::new().unwrap();
    let maki_dir = tmp.path().join(".maki");
    std::fs::create_dir_all(&maki_dir).unwrap();
    std::fs::write(
        maki_dir.join("init.lua"),
        format!(
            r#"maki.api.register_tool({{
            name = "{tool}",
            description = "{REPLACEMENT_DESC}",
            schema = {MINIMAL_SCHEMA},
            handler = function() return "" end
        }})"#
        ),
    )
    .unwrap();

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let mut warnings = Vec::new();
    host.load_init_files_or_skip(false, tmp.path(), &mut warnings)
        .expect("init.lua must load");

    assert!(reg.has(tool));
    assert_eq!(
        warnings
            .iter()
            .filter(|w| w.contains(PERMISSION_NAME_WARNING) && w.contains(tool))
            .count(),
        expected,
        "got: {warnings:?}"
    );
}

#[test]
fn require_caches_modules() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(lua_dir.join("counter.lua"), "return { value = 42 }\n").unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
local a = require("counter")
local b = require("counter")
assert(a == b, "require should return cached module")
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_plugin_file(&init_path).unwrap();
}

#[test]
fn require_sandbox_escape_blocked() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(tmp.path().join("init.lua"), "require(\"../../escape\")\n").unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_plugin_file(&init_path)
        .expect_err("expected sandbox error");
    assert!(matches!(err, PluginError::Lua { .. }));
    let msg = err.to_string();
    assert!(
        msg.contains("sandbox") || msg.contains("outside"),
        "got: {msg}"
    );
}

/// Neovim resolves `lua/foo/init.lua` as well as `lua/foo.lua`, and an
/// external package laid out the Neovim way relies on it.
#[test]
fn require_resolves_directory_init_form() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mod_dir = tmp.path().join("lua").join("pkg");
    std::fs::create_dir_all(&mod_dir).unwrap();

    std::fs::write(mod_dir.join("init.lua"), "return { value = 7 }\n").unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
local pkg = require("pkg")
assert(pkg.value == 7, "expected lua/pkg/init.lua to resolve")
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_plugin_file(&init_path).unwrap();
}

/// `<mod>.lua` wins over `<mod>/init.lua`, matching Neovim's order.
#[test]
fn require_prefers_flat_module_over_directory_init() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(lua_dir.join("pkg")).unwrap();

    std::fs::write(lua_dir.join("pkg.lua"), "return { which = \"flat\" }\n").unwrap();
    std::fs::write(
        lua_dir.join("pkg").join("init.lua"),
        "return { which = \"dir\" }\n",
    )
    .unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
local pkg = require("pkg")
assert(pkg.which == "flat", "expected pkg.lua to win, got " .. tostring(pkg.which))
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_plugin_file(&init_path).unwrap();
}

/// A git repository can commit a symlink, so the lexical `..` check is not
/// enough on its own: the resolved path has to be re-checked.
#[cfg(unix)]
#[test]
fn require_symlink_out_of_package_blocked() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    let outside = tmp.path().join("outside.lua");
    std::fs::write(&outside, "return { secret = true }\n").unwrap();
    std::os::unix::fs::symlink(&outside, lua_dir.join("leak.lua")).unwrap();

    std::fs::write(tmp.path().join("init.lua"), "require(\"leak\")\n").unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_plugin_file(&init_path)
        .expect_err("symlink pointing out of the package must not load");
    let msg = err.to_string();
    assert!(
        msg.contains("sandbox") || msg.contains("outside"),
        "got: {msg}"
    );
}

#[cfg(unix)]
#[test]
fn global_init_can_require_a_symlinked_module() {
    let config = tempfile::TempDir::new().unwrap();
    let modules = config.path().join("lua");
    std::fs::create_dir_all(&modules).unwrap();
    let elsewhere = tempfile::TempDir::new().unwrap();
    let target = elsewhere.path().join("shared.lua");
    std::fs::write(&target, "return { value = 42 }\n").unwrap();
    std::os::unix::fs::symlink(&target, modules.join("shared.lua")).unwrap();

    let host = PluginHost::new(fresh_registry()).unwrap();
    let _ = host
        .send_global_init_lua(
            "assert(require('shared').value == 42)".to_owned(),
            Some(config.path().to_path_buf()),
        )
        .unwrap();
}

#[cfg(unix)]
#[test]
fn global_init_can_use_a_symlinked_lua_directory() {
    let config = tempfile::TempDir::new().unwrap();
    let elsewhere = tempfile::TempDir::new().unwrap();
    std::fs::write(elsewhere.path().join("shared.lua"), "return true\n").unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), config.path().join("lua")).unwrap();

    let host = PluginHost::new(fresh_registry()).unwrap();
    let _ = host
        .send_global_init_lua(
            "assert(require('shared'))".to_owned(),
            Some(config.path().to_path_buf()),
        )
        .unwrap();
}

#[test]
fn require_circular_returns_sentinel_and_caches_real_value() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(
        lua_dir.join("a.lua"),
        "local b = require(\"b\")\nreturn { name = \"a\" }\n",
    )
    .unwrap();
    std::fs::write(
        lua_dir.join("b.lua"),
        "local a = require(\"a\")\nassert(a == true, \"circular require should return sentinel\")\nreturn { name = \"b\" }\n",
    )
    .unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
require("a")
local a2 = require("a")
assert(type(a2) == "table", "cached value should be table, got: " .. type(a2))
assert(a2.name == "a", "cached value should have name='a'")
local b2 = require("b")
assert(type(b2) == "table", "cached value should be table, got: " .. type(b2))
assert(b2.name == "b", "cached value should have name='b'")
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_plugin_file(&init_path).unwrap();
}

#[test]
fn require_nonexistent_module_errors() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(tmp.path().join("init.lua"), "require(\"nonexistent\")\n").unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_plugin_file(&init_path)
        .expect_err("expected error for missing module");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains("nonexistent"), "got: {err}");
}

#[test]
fn require_error_cleans_loading_state() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(lua_dir.join("bad.lua"), "error('deliberate')").unwrap();
    std::fs::write(lua_dir.join("good.lua"), "return { ok = true }").unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
local ok, err = pcall(require, "bad")
assert(not ok, "bad module should fail")

-- second require of the same broken module must error again, not return a sentinel
local ok2, err2 = pcall(require, "bad")
assert(not ok2, "broken module should fail on retry too")

-- unrelated modules must still work
local g = require("good")
assert(type(g) == "table", "good module should load, got: " .. type(g))
assert(g.ok == true)
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_plugin_file(&init_path).unwrap();
}

#[test]
fn multi_tool_plugin_registers_and_unloads_all() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"
maki.api.register_tool({{
    name = "multi_alpha",
    description = "first tool",
    schema = {MINIMAL_SCHEMA},
    handler = function() return "alpha" end
}})
maki.api.register_tool({{
    name = "multi_beta",
    description = "second tool",
    schema = {MINIMAL_SCHEMA},
    handler = function() return "beta" end
}})
"#,
    );
    host.load_source("multi", &src).unwrap();

    assert!(reg.has("multi_alpha"));
    assert!(reg.has("multi_beta"));

    host.unload("multi").unwrap();
    assert!(!reg.has("multi_alpha"));
    assert!(!reg.has("multi_beta"));
}

#[test]
fn conflict_from_different_plugin_preserves_original() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "evolving",
            description = "version 1",
            schema = {MINIMAL_SCHEMA},
            handler = function() return "v1" end
        }})"#,
    );
    host.load_source("keeper", &src).unwrap();
    assert!(reg.has("evolving"));

    let err = host
        .load_source("intruder", &src)
        .expect_err("expected conflict");
    assert!(matches!(err, PluginError::NameConflict { .. }));

    let entry = reg.get("evolving").unwrap();
    assert!(matches!(entry.source, ToolSource::Lua { ref plugin } if plugin.as_ref() == "keeper"),);
}

#[test]
fn ctx_finish_called_twice_is_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "double_finish",
            description = "calls finish twice",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                ctx:finish("first")
                ctx:finish("second")
            end
        }})"#,
    );
    host.load_source("double_finish", &src).unwrap();
    let err = exec_tool(&reg, "double_finish", serde_json::json!({})).unwrap_err();
    assert!(err.contains(FINISH_CALLED_TWICE_ERR), "got: {err}");
}

#[test]
fn ctx_finish_with_is_error_propagates() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "finish_err",
            description = "finishes with error",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                ctx:finish({{ llm_output = "async boom", is_error = true }})
            end
        }})"#,
    );
    host.load_source("finish_err", &src).unwrap();
    let err = exec_tool(&reg, "finish_err", serde_json::json!({})).unwrap_err();
    assert_eq!(err, "async boom");
}

#[test]
fn async_job_on_exit_receives_exit_code() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "job_exit_code",
            description = "reports exit code",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                maki.fn.jobstart("exit 42", {{
                    on_exit = function(job_id, code)
                        ctx:finish("code=" .. tostring(code))
                    end
                }})
            end
        }})"#,
    );
    host.load_source("job_exit_code", &src).unwrap();
    let out = exec_tool(&reg, "job_exit_code", serde_json::json!({})).unwrap();
    assert_eq!(out, "code=42");
}

#[test]
fn jobwait_fires_callbacks_while_waiting() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "job_stream",
            description = "streams lines during jobwait",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local seen = {{}}
                local exit_code
                local id = maki.fn.jobstart("echo a; echo b; exit 7", {{
                    on_stdout = function(_, line) seen[#seen + 1] = line end,
                    on_exit = function(_, code) exit_code = code end,
                }})
                local res = maki.fn.jobwait(id)
                return table.concat(seen, ",")
                    .. " exit=" .. tostring(exit_code)
                    .. " stdout=" .. (res.stdout:gsub("\n", ","))
            end
        }})"#,
    );
    host.load_source("job_stream", &src).unwrap();
    let out = exec_tool(&reg, "job_stream", serde_json::json!({})).unwrap();
    assert_eq!(out, "a,b exit=7 stdout=a,b");
}

#[test]
fn jobstart_invalid_cwd_errors_with_expanded_path() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "job_bad_cwd",
            description = "jobstart with missing tilde cwd",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local _, err = pcall(maki.fn.jobstart, "pwd", {{ cwd = "{JOB_BAD_CWD}" }})
                return tostring(err)
            end
        }})"#,
    );
    host.load_source("job_bad_cwd", &src).unwrap();
    let out = exec_tool(&reg, "job_bad_cwd", serde_json::json!({})).unwrap();
    let expanded = maki_storage::paths::home()
        .expect("home dir")
        .join(JOB_BAD_CWD.strip_prefix("~/").unwrap());
    let expected = format!("{JOB_BAD_CWD_ERR_PREFIX}{}", expanded.display());
    assert!(out.contains(&expected), "got: {out}");
}

#[test]
fn async_job_exits_without_finish_is_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "job_no_finish",
            description = "job exits but never calls finish",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                maki.fn.jobstart("echo oops", {{
                    on_exit = function(job_id, code) end
                }})
            end
        }})"#,
    );
    host.load_source("job_no_finish", &src).unwrap();
    let err = exec_tool(&reg, "job_no_finish", serde_json::json!({})).unwrap_err();
    assert!(err.contains(NIL_WITHOUT_JOBS_ERR), "got: {err}");
}

#[test]
fn async_job_callback_error_surfaces() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "job_cb_err",
            description = "callback throws",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                maki.fn.jobstart("echo trigger", {{
                    on_exit = function(job_id, code)
                        error("callback exploded")
                    end
                }})
            end
        }})"#,
    );
    host.load_source("job_cb_err", &src).unwrap();
    let err = exec_tool(&reg, "job_cb_err", serde_json::json!({})).unwrap_err();
    assert!(err.contains("callback exploded"), "got: {err}");
}

/// Runs `tool`, whose handler parks on `jobstart("sleep 30")` until a
/// click lands, while this thread keeps re-sending clicks until it
/// finishes. Clicks are fire-and-forget, so the loop self-corrects: only a
/// click delivered while the handler is registered can finish the tool.
fn click_until_finished(
    host: &PluginHost,
    reg: &ToolRegistry,
    tool: &str,
    click_id: &'static str,
) -> String {
    let eh = host.event_handle();
    let entry = reg.get(tool).expect("tool registered");
    let inv = entry.tool.parse(&serde_json::json!({})).expect("parse");
    let worker = std::thread::spawn(move || {
        let ctx = maki_agent::tools::test_support::stub_ctx_with(
            &maki_agent::AgentMode::Build,
            None,
            Some(click_id),
        );
        smol::block_on(inv.execute(&ctx)).output
    });
    for _ in 0..500 {
        if worker.is_finished() {
            break;
        }
        eh.request_click(click_id.to_owned(), 0);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let out = worker.join().expect("worker thread").expect("tool output");
    match out {
        maki_agent::ToolOutput::Plain(s) => s.text,
        other => panic!("unexpected output: {other:?}"),
    }
}

#[test]
fn live_click_reaches_running_tool() {
    const LIVE_CLICK_ID: &str = "live-click-1";
    const CLICKED_MSG: &str = "clicked";
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "live_click",
            description = "finishes when clicked while running",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local buf = maki.ui.buf()
                buf:on("click", function()
                    ctx:finish("{CLICKED_MSG}")
                end)
                maki.fn.jobstart("sleep 30", {{}})
            end
        }})"#,
    );
    host.load_source("live_click", &src).unwrap();
    assert_eq!(
        click_until_finished(&host, &reg, "live_click", LIVE_CLICK_ID),
        CLICKED_MSG
    );
}

/// With several bufs holding click handlers, `request_click` must reach
/// the buf passed to `ctx:live_buf` (the root), not the first-created
/// fallback.
#[test]
fn live_click_routes_to_root_buf_among_many() {
    const ROOT_CLICK_ID: &str = "root-click-1";
    const ROOT_MSG: &str = "root_clicked";
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "root_click",
            description = "decoy buf registers a click first",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local decoy = maki.ui.buf()
                decoy:on("click", function() ctx:finish("decoy_clicked") end)
                local root = maki.ui.buf()
                root:on("click", function() ctx:finish("{ROOT_MSG}") end)
                ctx:live_buf(root)
                maki.fn.jobstart("sleep 30", {{}})
            end
        }})"#,
    );
    host.load_source("root_click", &src).unwrap();
    assert_eq!(
        click_until_finished(&host, &reg, "root_click", ROOT_CLICK_ID),
        ROOT_MSG
    );
}

const WARM_TOOL_NAME: &str = "warm_probe";
const WARM_INITIAL_LINE: &str = "initial";
const WARM_CLICK_LINE: &str = "warm_clicked";
const WARM_ERROR_OUTPUT: &str = "boom";
const WARM_RESTORED_LINE: &str = "restored";
const WARM_RESTORE_CLICK_LINE: &str = "restore_clicked";

/// `live_click` wires the handler-side click; restore always wires its own.
fn warm_host(is_error: bool, live_click: bool) -> (Arc<ToolRegistry>, PluginHost) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let ret = if is_error {
        format!(r#"{{ llm_output = "{WARM_ERROR_OUTPUT}", is_error = true }}"#)
    } else {
        r#""done""#.to_owned()
    };
    let on_click = if live_click {
        format!(
            r#"buf:on("click", function()
                    buf:set_lines({{ "{WARM_CLICK_LINE}" }})
                end)"#
        )
    } else {
        String::new()
    };
    let src = format!(
        r#"maki.api.register_tool({{
            name = "{WARM_TOOL_NAME}",
            description = "warm click probe",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local buf = maki.ui.buf()
                buf:set_lines({{ "{WARM_INITIAL_LINE}" }})
                {on_click}
                ctx:live_buf(buf)
                return {ret}
            end,
            restore = function(input, output, is_error, rctx)
                local buf = maki.ui.buf()
                buf:set_lines({{ "{WARM_RESTORED_LINE}" }})
                buf:on("click", function()
                    buf:set_lines({{ "{WARM_RESTORE_CLICK_LINE}" }})
                end)
                return {{ body = buf }}
            end
        }})"#,
    );
    host.load_source("warm_probe_plugin", &src).unwrap();
    (reg, host)
}

/// `load_source` waits for the request channel and the inflight gate, so
/// once it returns every click sent before it has fully run, async jobs
/// included. No sleeps needed. It also clears the warm map, so click
/// before the barrier, never after.
fn barrier(host: &PluginHost) {
    host.load_source("barrier", "").unwrap();
}

fn warm_restore_item(id: &str, clicks: Vec<usize>) -> maki_lua::RestoreItem {
    maki_lua::RestoreItem {
        tool: Arc::from(WARM_TOOL_NAME),
        tool_use_id: id.to_owned(),
        output: "done".to_owned(),
        input: serde_json::json!({}),
        is_error: false,
        tool_output_lines: ToolOutputLines::default(),
        theme_gen: None,
        clicks,
        state: None,
    }
}

fn snapshot_texts(rx: &flume::Receiver<maki_agent::Envelope>, id: &str) -> Vec<String> {
    rx.drain()
        .filter_map(|env| match env.event {
            maki_agent::AgentEvent::ToolSnapshot {
                id: got, snapshot, ..
            } if got == id => Some(
                snapshot
                    .lines
                    .iter()
                    .flat_map(|l| l.spans.iter().map(|s| s.text.clone()))
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

fn warm_ctx(
    id: &str,
) -> (
    maki_agent::tools::ToolContext,
    flume::Receiver<maki_agent::Envelope>,
) {
    let (tx, rx) = flume::unbounded::<maki_agent::Envelope>();
    let event_tx = maki_agent::EventSender::new(tx, 0);
    let ctx = maki_agent::tools::test_support::stub_ctx_with(
        &maki_agent::AgentMode::Build,
        Some(&event_tx),
        Some(id),
    );
    (ctx, rx)
}

fn exec_warm_tool(
    reg: &ToolRegistry,
    tool: &str,
    ctx: &maki_agent::tools::ToolContext,
) -> Result<maki_agent::ToolOutput, String> {
    let inv = reg
        .get(tool)
        .expect("tool registered")
        .tool
        .parse(&serde_json::json!({}))
        .expect("parse failed");
    smol::block_on(inv.execute(ctx)).output
}

/// A click on a finished tool takes the warm path: it mutates the live
/// root buf and the fallback restore stays unused. Failed tools stay
/// warm too, since people click them to see what went wrong.
#[test_case::test_case(false ; "success")]
#[test_case::test_case(true ; "error_finish")]
fn warm_click_reaches_finished_tool(is_error: bool) {
    const WARM_ID: &str = "warm-click-1";
    let (reg, host) = warm_host(is_error, true);
    let (ctx, rx) = warm_ctx(WARM_ID);
    let res = exec_warm_tool(&reg, WARM_TOOL_NAME, &ctx);
    assert_eq!(res.err(), is_error.then(|| WARM_ERROR_OUTPUT.to_owned()));
    let body = recv_live_buf(&rx, WARM_ID).expect("live buf published");

    let (fb_tx, fb_rx) = flume::unbounded();
    let eh = host.event_handle();
    eh.request_click_with_fallback(
        WARM_ID.to_owned(),
        0,
        warm_restore_item(WARM_ID, vec![0]),
        maki_agent::EventSender::new(fb_tx, 0),
    );
    barrier(&host);

    assert_eq!(body.read()[0].spans[0].text, WARM_CLICK_LINE);
    assert!(
        snapshot_texts(&fb_rx, WARM_ID).is_empty(),
        "warm hit must not trigger the fallback restore"
    );
}

/// A click that misses both the live and warm maps restores from the
/// fallback item (replaying its recorded clicks), so an evicted or
/// desynced warm cache costs latency, never a dropped click.
#[test]
fn click_fallback_restores_when_warm_missing() {
    const GONE_ID: &str = "warm-gone-1";
    let (_reg, host) = warm_host(false, true);
    let (tx, rx) = flume::unbounded();

    let eh = host.event_handle();
    eh.request_click_with_fallback(
        GONE_ID.to_owned(),
        0,
        warm_restore_item(GONE_ID, vec![0]),
        maki_agent::EventSender::new(tx, 0),
    );
    barrier(&host);

    assert_eq!(
        snapshot_texts(&rx, GONE_ID),
        vec![WARM_RESTORE_CLICK_LINE.to_owned()],
        "fallback restore must replay the recorded clicks"
    );
}

/// A warm hit whose root buf has no click handler must still consume
/// the fallback: some plugins wire clicks only in `restore`.
#[test]
fn click_fallback_restores_when_warm_buf_has_no_handler() {
    const WARM_ID: &str = "warm-nohandler-1";
    let (reg, host) = warm_host(false, false);
    let (ctx, rx) = warm_ctx(WARM_ID);
    exec_warm_tool(&reg, WARM_TOOL_NAME, &ctx).expect("tool output");
    recv_live_buf(&rx, WARM_ID).expect("live buf published");

    let (fb_tx, fb_rx) = flume::unbounded();
    let eh = host.event_handle();
    eh.request_click_with_fallback(
        WARM_ID.to_owned(),
        0,
        warm_restore_item(WARM_ID, vec![0]),
        maki_agent::EventSender::new(fb_tx, 0),
    );
    barrier(&host);

    assert_eq!(
        snapshot_texts(&fb_rx, WARM_ID),
        vec![WARM_RESTORE_CLICK_LINE.to_owned()],
        "warm hit without a click handler must fall back to restore"
    );
}

/// Any restore of a tool supersedes its warm handle: the entry is
/// evicted so the stale view can never serve later clicks (e.g. with
/// old-theme content after a rebake).
#[test]
fn restore_evicts_warm_handle() {
    const WARM_ID: &str = "warm-rebaked-1";
    let (reg, host) = warm_host(false, true);
    let (ctx, rx) = warm_ctx(WARM_ID);
    exec_warm_tool(&reg, WARM_TOOL_NAME, &ctx).expect("tool output");
    let body = recv_live_buf(&rx, WARM_ID).expect("live buf published");

    let (tx, _rx) = flume::unbounded();
    let eh = host.event_handle();
    eh.request_restore(
        warm_restore_item(WARM_ID, Vec::new()),
        maki_agent::EventSender::new(tx, 0),
    );
    eh.request_click(WARM_ID.to_owned(), 0);
    barrier(&host);

    assert_eq!(
        body.read()[0].spans[0].text,
        WARM_INITIAL_LINE,
        "bare click after restore must be a no-op on the evicted warm buf"
    );
}

/// Overfilling the cache evicts the oldest entry. Bare clicks (no
/// fallback) make eviction observable: the evicted tool's click is
/// dropped while a still-warm one lands.
#[test]
fn warm_fifo_evicts_oldest_runtime_side() {
    let (reg, host) = warm_host(false, true);
    let mut bufs = Vec::with_capacity(WARM_TOOL_CAP + 1);
    for i in 0..=WARM_TOOL_CAP {
        let id = format!("t{i}");
        let (ctx, rx) = warm_ctx(&id);
        exec_warm_tool(&reg, WARM_TOOL_NAME, &ctx).expect("tool output");
        bufs.push(recv_live_buf(&rx, &id).expect("live buf published"));
    }

    let eh = host.event_handle();
    eh.request_click("t1".to_owned(), 0);
    eh.request_click("t0".to_owned(), 0);
    barrier(&host);

    assert_eq!(
        bufs[1].read()[0].spans[0].text,
        WARM_CLICK_LINE,
        "still-warm tool must take the warm click path"
    );
    assert_eq!(
        bufs[0].read()[0].spans[0].text,
        WARM_INITIAL_LINE,
        "evicted tool's click must be ignored"
    );
}

/// After a plugin (re)load the old handlers are gone, so stale warm
/// clicks must be dropped, never run.
#[test]
fn warm_map_cleared_by_load_source() {
    const WARM_ID: &str = "warm-cleared-1";
    let (reg, host) = warm_host(false, true);
    let (ctx, rx) = warm_ctx(WARM_ID);
    exec_warm_tool(&reg, WARM_TOOL_NAME, &ctx).expect("tool output");
    let body = recv_live_buf(&rx, WARM_ID).expect("live buf published");

    barrier(&host);
    let eh = host.event_handle();
    eh.request_click(WARM_ID.to_owned(), 0);
    barrier(&host);

    assert_eq!(body.read()[0].spans[0].text, WARM_INITIAL_LINE);
}

/// LoadSource's drain barrier spawns and awaits queued async jobs, so
/// jobs a warm click enqueues land before the barrier returns.
#[test]
fn warm_click_runs_async_jobs() {
    const WARM_ID: &str = "warm-async-1";
    const ASYNC_LINE: &str = "async_appended";
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "warm_async",
            description = "appends a line from an async job on click",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local buf = maki.ui.buf()
                buf:set_lines({{ "{WARM_INITIAL_LINE}" }})
                buf:on("click", function()
                    maki.async.run(function()
                        buf:line("{ASYNC_LINE}")
                    end)
                end)
                ctx:live_buf(buf)
                return "done"
            end
        }})"#,
    );
    host.load_source("warm_async_plugin", &src).unwrap();

    let (ctx, rx) = warm_ctx(WARM_ID);
    exec_warm_tool(&reg, "warm_async", &ctx).expect("tool output");
    let body = recv_live_buf(&rx, WARM_ID).expect("live buf published");

    let eh = host.event_handle();
    eh.request_click(WARM_ID.to_owned(), 0);
    barrier(&host);

    let text = body.take().text();
    assert!(text.contains(ASYNC_LINE), "async job line missing: {text}");
}

/// The warm cell gets a fresh `CancelToken::none()`: cancelling the
/// original run after it finished must not kill warm clicks.
#[test]
fn warm_click_survives_post_completion_cancel() {
    const WARM_ID: &str = "warm-cancel-1";
    let (reg, host) = warm_host(false, true);
    let (mut ctx, rx) = warm_ctx(WARM_ID);
    let (trigger, token) = maki_agent::CancelToken::new();
    ctx.cancel = token;
    exec_warm_tool(&reg, WARM_TOOL_NAME, &ctx).expect("tool output");
    let body = recv_live_buf(&rx, WARM_ID).expect("live buf published");
    trigger.cancel();

    let eh = host.event_handle();
    eh.request_click(WARM_ID.to_owned(), 0);
    barrier(&host);

    assert_eq!(body.read()[0].spans[0].text, WARM_CLICK_LINE);
}

/// `maki.agent.call_tool` returns `(text, err)` and delivers live bufs,
/// annotations (live and completion alike) and usage through the callbacks.
#[test]
fn call_tool_streams_live_buf_and_annotations() {
    let reg = fresh_registry();
    reg.register(
        Arc::new(UsageTool),
        ToolSource::Lua {
            plugin: Arc::from("usage_fixture"),
        },
    )
    .unwrap();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"
maki.api.register_tool({{
    name = "annotated_child",
    description = "returns an annotation",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx)
        return {{ llm_output = "child_done", annotation = "5 items" }}
    end
}})
maki.api.register_tool({{
    name = "streaming_child",
    description = "publishes a live buf then finishes",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx)
        local buf = maki.ui.buf()
        buf:line("streamed line")
        ctx:live_buf(buf)
        return "stream_done"
    end
}})
maki.api.register_tool({{
    name = "failing_child",
    description = "always errors",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx)
        return {{ llm_output = "boom", is_error = true }}
    end
}})
maki.api.register_tool({{
    name = "driver",
    description = "dispatches children via maki.agent.call_tool",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx)
        local ann = "nil"
        local text, err = maki.agent.call_tool(ctx, "annotated_child", {{}}, {{
            on_annotation = function(a) ann = a end,
        }})
        local live_text = "none"
        local ann2 = "nil"
        local text2 = maki.agent.call_tool(ctx, "streaming_child", {{}}, {{
            on_live_buf = function(b)
                local lines = b:get_lines()
                live_text = lines[1] and lines[1][1] and lines[1][1][1] or "empty"
            end,
            on_annotation = function(a) ann2 = a end,
        }})
        local usage = "nil"
        local text3 = maki.agent.call_tool(ctx, "{USAGE_TOOL_NAME}", {{}}, {{
            on_usage = function(value) usage = value end,
        }})
        local ann3 = "nil"
        local _, err3 = maki.agent.call_tool(ctx, "failing_child", {{}}, {{
            on_annotation = function(a) ann3 = a end,
        }})
        return tostring(text) .. "/" .. ann
            .. " " .. tostring(text2) .. "/" .. live_text .. "/" .. ann2
            .. " " .. tostring(text3) .. "/" .. usage
            .. " " .. tostring(err3) .. "/" .. ann3
    end
}})
"#,
    );
    host.load_source("call_tool_live", &src).unwrap();
    let out = exec_tool_in(
        &reg,
        "driver",
        serde_json::json!({}),
        Some(Arc::clone(&reg)),
    )
    .expect("driver ok");
    assert_eq!(
        out,
        format!(
            "child_done/5 items stream_done/streamed line/1 lines \
             {USAGE_OUTPUT}/{USAGE_VALUE} boom/nil"
        )
    );
}

#[test]
fn jobstop_kills_running_job() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "job_stop",
            description = "starts and immediately stops a job",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local id = maki.fn.jobstart("sleep 60", {{
                    on_exit = function(job_id, code)
                        ctx:finish("killed=" .. tostring(code ~= 0))
                    end
                }})
                maki.fn.jobstop(id)
            end
        }})"#,
    );
    host.load_source("job_stop", &src).unwrap();
    let out = exec_tool(&reg, "job_stop", serde_json::json!({})).unwrap();
    assert_eq!(out, "killed=true");
}

#[test]
fn plugin_owned_job_outlives_its_starting_tool() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"
local output = "pending"
local exit_code = "pending"
maki.api.register_tool({{
    name = "start_plugin_job",
    description = "starts a plugin-owned job",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        local id = maki.fn.jobstart("sleep 0.1; printf plugin-output; exit 7", {{
            scope = "plugin",
            on_stdout = function(_, line) output = line end,
            on_exit = function(_, code) exit_code = tostring(code) end,
        }})
        return maki.fn.jobwait(id, 1) == nil and "started" or "did not time out"
    end,
}})
maki.api.register_tool({{
    name = "plugin_job_state",
    description = "reports plugin-owned job callbacks",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        return output .. "/" .. exit_code
    end,
}})
"#
    );
    host.load_source("plugin_job", &src).unwrap();
    assert_eq!(
        exec_tool(&reg, "start_plugin_job", json!({})).unwrap(),
        "started"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let state = exec_tool(&reg, "plugin_job_state", json!({})).unwrap();
        if state == "plugin-output/7" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "plugin-owned callbacks did not run: {state}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn unloading_plugin_kills_its_jobs() {
    let host = PluginHost::new(fresh_registry()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pid_path = dir.path().join("job.pid");
    let src = format!(
        r#"maki.fn.jobstart("printf %s $$ > '{}'; exec sleep 30", {{
            scope = "plugin",
        }})"#,
        pid_path.display()
    );
    host.load_source("plugin_job", &src).unwrap();

    // The shell creates the redirect target before printf writes to it,
    // so poll until the file holds a parseable pid, not until it exists.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Ok(pid) = std::fs::read_to_string(&pid_path)
            .unwrap_or_default()
            .parse::<i32>()
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "plugin job did not publish its process id"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let pid = Pid::from_raw(pid).unwrap();
    assert!(test_kill_process_group(pid).is_ok());

    host.unload("plugin_job").unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while test_kill_process_group(pid).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "plugin process group survived unload"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The shell exits at once and leaves `sleep` holding the pipe, so the direct
/// child dies long before the job does. Reaping it there gives the pid back to
/// the kernel and turns every later kill into a no-op.
#[cfg(unix)]
#[test]
fn unloading_plugin_kills_a_job_whose_shell_already_exited() {
    let host = PluginHost::new(fresh_registry()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pid_path = dir.path().join("group.pid");
    let src = format!(
        r#"maki.fn.jobstart("sleep 30 & printf %s $$ > '{}'", {{
            scope = "plugin",
        }})"#,
        pid_path.display()
    );
    host.load_source("plugin_job", &src).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Ok(pid) = std::fs::read_to_string(&pid_path)
            .unwrap_or_default()
            .parse::<i32>()
        {
            break Pid::from_raw(pid).unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "plugin job did not publish its process group"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    host.unload("plugin_job").unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while test_kill_process_group(pid).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "backgrounded process survived unload"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn jobinfo_and_joblist_see_live_plugin_jobs() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"
local job_id
maki.api.register_tool({{
    name = "start_listed_job",
    description = "starts a plugin job for inspect",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        job_id = maki.fn.jobstart("printf 'hello-tail\n'; exec sleep 30", {{
            scope = "plugin",
            tail = 8,
        }})
        return tostring(job_id)
    end,
}})
maki.api.register_tool({{
    name = "inspect_listed_job",
    description = "jobinfo and joblist for the live job",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        local info = maki.fn.jobinfo(job_id)
        if not info then return "missing" end
        local listed = false
        for _, row in ipairs(maki.fn.joblist()) do
            if row.id == job_id then listed = true end
        end
        return table.concat({{
            info.status,
            info.command,
            tostring(listed),
            table.concat(info.stdout_lines, ","),
        }}, "|")
    end,
}})
maki.api.register_tool({{
    name = "stop_listed_job",
    description = "stop the inspect job",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        maki.fn.jobstop(job_id)
        return "stopped"
    end,
}})
"#
    );
    host.load_source("job_inspect", &src).unwrap();
    let _id = exec_tool(&reg, "start_listed_job", json!({})).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let state = loop {
        let state = exec_tool(&reg, "inspect_listed_job", json!({})).unwrap();
        if state.starts_with("running|") && state.contains("hello-tail") {
            break state;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "jobinfo never saw the live job: {state}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        state.contains("|true|"),
        "joblist should include the live job, got {state}"
    );

    exec_tool(&reg, "stop_listed_job", json!({})).unwrap();
}

#[cfg(unix)]
#[test]
fn session_owned_job_survives_plugin_reload() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let session = maki_storage::id::MakiId::generate();
    let _mailbox = maki_agent::SessionMailbox::register(session);
    let sid = session.to_string();
    let dir = tempfile::tempdir().unwrap();
    let pid_path = dir.path().join("job.pid");
    let src = format!(
        r#"
maki.api.register_tool({{
    name = "start_session_job",
    description = "starts a session-owned job",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        local id = maki.fn.jobstart("printf %s $$ > '{pid}'; exec sleep 30", {{
            scope = {{ session = "{sid}" }},
        }})
        return tostring(id)
    end,
}})
"#,
        pid = pid_path.display(),
        sid = sid,
    );
    host.load_source("session_job", &src).unwrap();
    let id = exec_tool(&reg, "start_session_job", json!({})).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Ok(pid) = std::fs::read_to_string(&pid_path)
            .unwrap_or_default()
            .parse::<i32>()
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session job did not publish its process id"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let pid = Pid::from_raw(pid).unwrap();
    assert!(test_kill_process_group(pid).is_ok());

    host.unload("session_job").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        test_kill_process_group(pid).is_ok(),
        "session-owned job must survive plugin unload"
    );

    let inspect = format!(
        r#"
maki.api.register_tool({{
    name = "inspect_session_job",
    description = "lists session jobs after reload",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        local info = maki.fn.jobinfo({id})
        if not info then return "missing" end
        return info.status .. ":" .. tostring(info.pid)
    end,
}})
"#
    );
    host.load_source("session_job", &inspect).unwrap();
    let state = exec_tool(&reg, "inspect_session_job", json!({})).unwrap();
    assert!(
        state.starts_with("running:"),
        "reloaded plugin should see the live session job, got {state}"
    );

    host.event_handle()
        .end_sessions_blocking([session], SessionEndReason::Shutdown);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while test_kill_process_group(pid).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "end_session must kill the session job"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn jobattach_re_arms_session_jobs_a_reload_dropped() {
    const EXIT_CODE: i32 = 3;
    const ATTACHED: &str = "attached";
    const NOT_FOUND: &str = "job: not found";
    // Status of the doomed job, whether ticks arrived, and the codes seen.
    const BEFORE_ATTACH: &str = "exited|false|";
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let session = maki_storage::id::MakiId::generate();
    let _mailbox = maki_agent::SessionMailbox::register(session);
    let sid = session.to_string();
    let starter = format!(
        r#"
maki.api.register_tool({{
    name = "start_jobs",
    description = "starts a chatty session job and one that dies at once",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        local ticker = maki.fn.jobstart("while true; do echo tick; sleep 0.05; done", {{
            scope = {{ session = "{sid}" }},
        }})
        local doomed = maki.fn.jobstart("exit {EXIT_CODE}", {{
            scope = {{ session = "{sid}" }},
        }})
        return ticker .. " " .. doomed
    end,
}})
"#
    );
    host.load_source("ticker", &starter).unwrap();
    let ids = exec_tool(&reg, "start_jobs", json!({})).unwrap();
    let (ticker, doomed) = ids.split_once(' ').expect("two job ids");

    let reattach = format!(
        r#"
local ticks, exits = 0, {{}}
maki.api.register_tool({{
    name = "attach_jobs",
    description = "re-arms the jobs the reload detached",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        local ok, err = maki.fn.jobattach({ticker}, {{
            on_stdout = function() ticks = ticks + 1 end,
        }})
        if not ok then return err end
        ok, err = maki.fn.jobattach({doomed}, {{
            on_exit = function(_, code) exits[#exits + 1] = code end,
        }})
        return ok and "{ATTACHED}" or err
    end,
}})
maki.api.register_tool({{
    name = "report",
    description = "what the re-armed callbacks have seen",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        local info = maki.fn.jobinfo({doomed})
        return (info and info.status or "missing")
            .. "|" .. tostring(ticks > 0)
            .. "|" .. table.concat(exits, ",")
    end,
}})
"#
    );
    host.load_source("ticker", &reattach).unwrap();
    poll_until("the doomed job never reported its exit", || {
        (exec_tool(&reg, "report", json!({})).unwrap() == BEFORE_ATTACH).then_some(())
    });
    assert_eq!(exec_tool(&reg, "attach_jobs", json!({})).unwrap(), ATTACHED);

    let expected = format!("exited|true|{EXIT_CODE}");
    poll_until("the re-armed callbacks never fired", || {
        (exec_tool(&reg, "report", json!({})).unwrap() == expected).then_some(())
    });

    let spy = format!(
        r#"
maki.api.register_tool({{
    name = "spy_attach",
    description = "attaches to another plugin's job",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        local ok, err = maki.fn.jobattach({ticker}, {{ on_stdout = function() end }})
        return ok and "{ATTACHED}" or err
    end,
}})
"#
    );
    host.load_source("spy", &spy).unwrap();
    assert_eq!(exec_tool(&reg, "spy_attach", json!({})).unwrap(), NOT_FOUND);

    host.event_handle()
        .end_sessions_blocking([session], SessionEndReason::Shutdown);
}

#[cfg(unix)]
#[test]
fn argv_jobs_and_stream_redirects() {
    const LITERAL_ARG: &str = "a; echo pwned $(id)";
    const REDIRECT_ERR: &str = "mutually exclusive";
    const FIELDS_ERR: &str = "handler must return the streams and the error, separated by |";
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("job.log");
    let src = format!(
        r#"
maki.api.register_tool({{
    name = "argv_and_redirect",
    description = "argv spawning plus stdout redirect",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        local seen = {{}}
        maki.fn.jobwait(maki.fn.jobstart({{ "echo", "{LITERAL_ARG}" }}, {{
            scope = "plugin",
            on_stdout = function(_, line) seen[#seen + 1] = line end,
        }}))

        local redirected = maki.fn.jobwait(maki.fn.jobstart({{ "echo", "to-file" }}, {{
            scope = "plugin",
            stdout = "{log}",
        }}))
        local quiet = maki.fn.jobwait(
            maki.fn.jobstart("echo dropped", {{ scope = "plugin", stdout = false }})
        )

        local _, err = pcall(maki.fn.jobstart, "echo both", {{
            scope = "plugin",
            stdout = "{log}",
            on_stdout = function() end,
        }})

        return table.concat({{
            table.concat(seen, ","),
            redirected.stdout,
            quiet.stdout,
            tostring(err),
        }}, "|")
    end,
}})
"#,
        log = log.display(),
    );
    host.load_source("argv_jobs", &src).unwrap();

    let out = exec_tool(&reg, "argv_and_redirect", json!({})).unwrap();
    let (streams, conflict) = out
        .rsplit_once('|')
        .unwrap_or_else(|| panic!("{FIELDS_ERR}"));
    assert_eq!(
        streams,
        format!("{LITERAL_ARG}||"),
        "argv must reach the program with no shell in between, and a stream sent to a file or dropped must not be captured too"
    );
    assert!(
        conflict.contains(REDIRECT_ERR),
        "redirect plus on_stdout must be refused, got {conflict}"
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "to-file\n");
}

#[cfg(unix)]
#[test]
fn jobwait_on_an_exited_job_reports_whether_its_tail_is_complete() {
    const WHOLE: &str = "one\ntwo:false";
    const CLIPPED: &str = "two:true";
    const DISCARDED: &str = ":true";
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let session = maki_storage::id::MakiId::generate();
    let _mailbox = maki_agent::SessionMailbox::register(session);
    let sid = session.to_string();
    let src = format!(
        r#"
maki.api.register_tool({{
    name = "wait_twice",
    description = "the tail three exited session jobs read back",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        -- The first wait parks until the exit and fills the tail, the second
        -- one answers from that tail, which is the path under test.
        local function tail_of(opts)
            opts.scope = {{ session = "{sid}" }}
            local id = maki.fn.jobstart("echo one; echo two", opts)
            maki.fn.jobwait(id)
            local got = maki.fn.jobwait(id)
            return got.stdout .. ":" .. tostring(got.truncated)
        end
        return table.concat({{
            tail_of({{ tail = 8 }}),
            tail_of({{ tail = 1 }}),
            tail_of({{ stdout = false }}),
        }}, "|")
    end,
}})
"#
    );
    host.load_source("chatty", &src).unwrap();

    assert_eq!(
        exec_tool(&reg, "wait_twice", json!({})).unwrap(),
        format!("{WHOLE}|{CLIPPED}|{DISCARDED}"),
        "a tail that held everything is not truncated, an empty one we never filled is"
    );

    host.event_handle()
        .end_sessions_blocking([session], SessionEndReason::Shutdown);
}

/// `run` on its own is enough to start a job, but pointing a stream at a path
/// is a write, so it costs `fs_write` too.
#[test]
fn stream_redirect_to_a_path_needs_fs_write() {
    const REDIRECT_TOOL: &str = "redirect_deny";
    let mut perms = maki_lua::PluginPermissions::denied();
    perms.set(maki_lua::Permission::Run, true);
    let src = perm_tool_src(
        REDIRECT_TOOL,
        r#"local _, err = pcall(maki.fn.jobstart, "echo hi", { scope = "plugin", stdout = "/tmp/maki-never-written.log" })
                return tostring(err)"#,
    );

    let result = exec_tool_with_perms(perms, &src, REDIRECT_TOOL, json!({})).unwrap();

    assert!(result.contains(PERMISSION_DENIED_MSG), "got: {result}");
    assert!(result.contains("fs_write"), "got: {result}");
}

#[cfg(unix)]
#[test]
fn a_job_name_survives_a_reload_and_blocks_a_second_live_job() {
    const JOB_NAME: &str = "log-tail";
    const DUPLICATE_ERR: &str = "already held by live job";
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let session = maki_storage::id::MakiId::generate();
    let _mailbox = maki_agent::SessionMailbox::register(session);
    let sid = session.to_string();
    let src = format!(
        r#"
maki.api.register_tool({{
    name = "start_named",
    description = "starts a named session job twice",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        local opts = {{ scope = {{ session = "{sid}" }}, name = "{JOB_NAME}" }}
        local id = maki.fn.jobstart("exec sleep 30", opts)
        local _, err = pcall(maki.fn.jobstart, "exec sleep 30", opts)
        return tostring(id) .. "|" .. tostring(err)
    end,
}})
"#
    );
    host.load_source("named", &src).unwrap();
    let started = exec_tool(&reg, "start_named", json!({})).unwrap();
    let (id, dup_err) = started.split_once('|').expect("id and duplicate error");
    assert!(
        dup_err.contains(DUPLICATE_ERR),
        "a second live job under the same name must be refused, got {dup_err}"
    );

    let rediscover = format!(
        r#"
maki.api.register_tool({{
    name = "find_named",
    description = "finds the surviving job by name",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function() return tostring(maki.fn.jobfind("{JOB_NAME}")) end,
}})
"#
    );
    host.load_source("named", &rediscover).unwrap();
    assert_eq!(
        exec_tool(&reg, "find_named", json!({})).unwrap(),
        id,
        "a name must survive the reload that dropped the callbacks"
    );

    host.event_handle()
        .end_sessions_blocking([session], SessionEndReason::Shutdown);
}

#[cfg(unix)]
#[test]
fn session_end_handler_sees_jobs_before_they_are_reaped() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let session = maki_storage::id::MakiId::generate();
    let _mailbox = maki_agent::SessionMailbox::register(session);
    let sid = session.to_string();
    let dir = tempfile::tempdir().unwrap();
    let pid_path = dir.path().join("job.pid");
    let src = format!(
        r#"
maki.api.register_tool({{
    name = "start_order_job",
    description = "starts a session-owned job",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        job_id = maki.fn.jobstart("printf %s $$ > '{pid}'; exec sleep 30", {{
            scope = {{ session = "{sid}" }},
        }})
        return tostring(job_id)
    end,
}})
maki.api.register_tool({{
    name = "probe_order_job",
    description = "reports what the SessionEnd handler saw",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        return seen or "not-yet"
    end,
}})
maki.api.create_autocmd("SessionEnd", {{
    callback = function(ev)
        if tostring(ev.data and ev.data.session_id) ~= "{sid}" then return end
        local ok, info = pcall(maki.fn.jobinfo, job_id)
        if not ok then
            seen = "err:" .. tostring(info)
            return
        end
        seen = info and (info.status .. ":" .. tostring(info.pid)) or "missing"
    end,
}})
local seen
"#,
        pid = pid_path.display(),
        sid = sid,
    );
    host.load_source("order_probe", &src).unwrap();
    exec_tool(&reg, "start_order_job", json!({})).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Ok(pid) = std::fs::read_to_string(&pid_path)
            .unwrap_or_default()
            .parse::<i32>()
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session job did not publish its process id"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let pid = Pid::from_raw(pid).unwrap();
    assert!(test_kill_process_group(pid).is_ok());

    host.event_handle()
        .end_sessions_blocking([session], SessionEndReason::Shutdown);

    // `end_sessions_blocking` only answers once the handlers ran and the jobs
    // were reaped, so what the handler saw is settled by now.
    let seen = exec_tool(&reg, "probe_order_job", json!({})).unwrap();
    assert!(
        seen.starts_with("running:"),
        "SessionEnd handler should see the live job, got {seen}"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while test_kill_process_group(pid).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "end_session must reap the job after dispatching SessionEnd"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn autocmd_task_jobs_die_with_their_own_callback() {
    let (reg, host) = builtins_host();
    const GONE: &str = "gone";
    let src = format!(
        r#"
local job
seen = "unset"
maki.api.create_autocmd("ProbeIsolation", {{
    callback = function()
        job = maki.fn.jobstart("sleep 30")
    end,
}})
maki.api.create_autocmd("ProbeIsolation", {{
    callback = function()
        local info = job and maki.fn.jobinfo(job) or nil
        seen = info and ("alive:" .. info.status) or "{GONE}"
    end,
}})
maki.api.register_tool({{
    name = "probe_isolation",
    description = "reports what the second handler saw",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        return seen
    end,
}})
"#
    );
    host.load_source("isolation_probe", &src).unwrap();

    host.event_handle()
        .fire_autocmd("ProbeIsolation", json!({}));

    // FireAutocmd and CallTool queue on the same channel and dispatch is
    // awaited in order, so the second handler has already run here. A shared
    // batch scope would report the first handler's job as alive.
    assert_eq!(exec_tool(&reg, "probe_isolation", json!({})).unwrap(), GONE);
}

#[cfg(unix)]
#[test]
fn session_end_autocmds_may_suspend() {
    let (reg, host) = builtins_host();

    let dir = std::env::temp_dir().join(format!("maki-sessionend-suspend-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("marker.txt");
    std::fs::write(&marker, "x").unwrap();

    const RM_FAILED: &str = "err:";
    let src = format!(
        r#"
local rm_result
maki.api.create_autocmd("SessionEnd", {{
    callback = function(ev)
        local ok, res = pcall(maki.fs.rm, ev.data.dir, {{ recursive = true, force = true }})
        rm_result = ok and "ok" or "{RM_FAILED}" .. tostring(res)
    end,
}})
maki.api.register_tool({{
    name = "rm_probe",
    description = "reports what the SessionEnd handler saw",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        return rm_result or "unset"
    end,
}})
"#,
    );
    host.load_source("sessionend_suspend", &src).unwrap();

    host.event_handle()
        .fire_autocmd("SessionEnd", json!({ "dir": dir.display().to_string() }));

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let seen = loop {
        match exec_tool(&reg, "rm_probe", json!({})) {
            Ok(seen) if seen != "unset" => break seen,
            _ if std::time::Instant::now() < deadline => {}
            other => panic!("SessionEnd probe never settled: {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(seen, "ok", "fs.rm must not die on a yield boundary");
    assert!(!marker.exists(), "fs.rm should have removed the tree");

    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn jobwait_streams_events_to_suspending_callbacks() {
    let (reg, host) = builtins_host();

    let dir = std::env::temp_dir().join(format!("maki-jobwait-suspend-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let meta = dir.join("meta.json");

    let session = maki_storage::id::MakiId::generate();
    // The process must still run when wait_suspending_job calls jobwait:
    // an already-exited job answers from its snapshot without delivering
    // on_exit, which would make the assertion below race the event pump.
    const EXIT_CB_FAILED: &str = "exit_cb_failed";
    let src = format!(
        r#"
local job_id
local exit_cb_result
maki.api.register_tool({{
    name = "start_suspending_job",
    description = "starts a session-owned job whose on_exit writes a file",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        job_id = maki.fn.jobstart("sleep 2", {{
            scope = {{ session = "{session}" }},
            on_exit = function(_, code)
                local ok, res = pcall(maki.fs.atomic_write, "{}", tostring(code))
                exit_cb_result = ok and "ok" or "{EXIT_CB_FAILED}:" .. tostring(res)
            end,
        }})
        return tostring(job_id)
    end,
}})
maki.api.register_tool({{
    name = "wait_suspending_job",
    description = "waits like monitor_wait does",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        local ok, res = pcall(maki.fn.jobwait, job_id, 10000)
        if not ok then
            return {{ llm_output = "error: " .. tostring(res), is_error = true }}
        end
        return "exit:" .. tostring(res and res.exit_code) .. "|exit_cb:" .. tostring(exit_cb_result)
    end,
}})
"#,
        meta.display(),
    );
    host.load_source("jobwait_suspend", &src).unwrap();

    exec_tool(&reg, "start_suspending_job", json!({})).unwrap();
    let waited = exec_tool(&reg, "wait_suspending_job", json!({})).unwrap();
    assert_eq!(
        waited, "exit:0|exit_cb:ok",
        "jobwait must report the exit and on_exit must survive suspending fs calls"
    );
    assert!(
        std::path::Path::new(&meta).exists(),
        "on_exit should have written the meta file"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn vm_recovers_after_async_job_tool() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"
maki.api.register_tool({{
    name = "async_first",
    description = "async tool",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx)
        maki.fn.jobstart("echo hi", {{
            on_exit = function(job_id, code) ctx:finish("ok1") end
        }})
    end
}})
maki.api.register_tool({{
    name = "sync_after",
    description = "sync tool",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function() return "ok2" end
}})
"#,
    );
    host.load_source("recovery", &src).unwrap();
    let out1 = exec_tool(&reg, "async_first", serde_json::json!({})).unwrap();
    assert_eq!(out1, "ok1");
    let out2 = exec_tool(&reg, "sync_after", serde_json::json!({})).unwrap();
    assert_eq!(out2, "ok2");
}

#[test]
fn setup_happy_path() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let raw = host
        .send_run_init_lua(
            "maki.setup({ agent = { max_output_lines = 3000 } })".to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap();
    let raw = raw.expect("expected Some(RawConfig)");
    assert_eq!(raw.agent.max_output_lines, Some(3000));
}

#[test]
fn project_init_cannot_declare_global_packages() {
    let host = PluginHost::new(fresh_registry()).unwrap();

    let error = host
        .send_run_init_lua(
            r#"maki.pack.add({ "https://example.com/demo" })"#.to_owned(),
            "project/init.lua".to_owned(),
            None,
        )
        .expect_err("project config must not change global packages");

    assert!(
        error.to_string().contains(GLOBAL_PACK_ONLY_ERR),
        "got: {error}"
    );
}

#[test]
fn a_named_config_cannot_change_global_packages() {
    let host = PluginHost::new(fresh_registry()).unwrap();

    let error = host
        .send_run_init_lua(
            r#"maki.pack.add({ "https://example.com/demo" })"#.to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .expect_err("only the global config may change packages");

    assert!(
        error.to_string().contains(GLOBAL_PACK_ONLY_ERR),
        "got: {error}"
    );
}

#[test]
fn global_init_can_declare_managed_packages() {
    let host = PluginHost::new(fresh_registry()).unwrap();

    let _ = host
        .send_global_init_lua(
            r#"maki.pack.add({ "https://example.com/demo" })"#.to_owned(),
            None,
        )
        .unwrap();

    let declared = host.declared_packages().unwrap();
    assert_eq!(declared.len(), 1);
    assert_eq!(declared[0].spec.name, "demo");
}

#[test]
fn project_init_can_activate_an_installed_global_package() {
    let host = PluginHost::new(fresh_registry()).unwrap();

    let _ = host
        .send_run_init_lua(
            r#"maki.packadd("demo")"#.to_owned(),
            "project/init.lua".to_owned(),
            None,
        )
        .unwrap();

    assert_eq!(
        host.seal_pack_ops().unwrap(),
        [maki_lua::PackOp::Activate {
            name: "demo".to_owned()
        }]
    );
}

#[test_case::test_case(
    r#"maki.setup({ agent = { compaction_buffer = 10000 } })"#,
    maki_config::CompactionBuffer::Tokens(10_000)
    ; "compaction_buffer_tokens"
)]
#[test_case::test_case(
    r#"maki.setup({ agent = { compaction_buffer = "15%" } })"#,
    maki_config::CompactionBuffer::Percent(15)
    ; "compaction_buffer_percent"
)]
fn setup_compaction_buffer(lua_src: &str, expected: maki_config::CompactionBuffer) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let raw = host
        .send_run_init_lua(lua_src.to_owned(), "test_init.lua".to_owned(), None)
        .unwrap()
        .expect("expected Some(RawConfig)");
    assert_eq!(raw.agent.compaction_buffer, Some(expected));
}

#[test_case::test_case(
    "maki.setup({ ui = { splash_animaton = false } })",
    UNKNOWN_FIELD_ERR
    ; "unknown_field"
)]
#[test_case::test_case(
    r#"maki.setup({ agent = { max_output_lines = "not a number" } })"#,
    ""
    ; "wrong_type"
)]
#[test_case::test_case(
    "maki.setup({ agent = { bash_timeout_secs = 120 } })",
    UNKNOWN_FIELD_ERR
    ; "moved_plugin_option"
)]
#[test_case::test_case(
    r#"maki.setup({ provider = { allowed_models = "anthropic/*" } })"#,
    ""
    ; "model_policy_wrong_type"
)]
fn setup_rejects_bad_input(lua_src: &str, expected_substr: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .send_run_init_lua(lua_src.to_owned(), "test_init.lua".to_owned(), None)
        .expect_err("expected error");
    assert!(matches!(err, PluginError::Lua { .. }), "got: {err}");
    if !expected_substr.is_empty() {
        assert!(err.to_string().contains(expected_substr), "got: {err}");
    }
}

#[test]
fn setup_model_policy_lists() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let raw = host
        .send_run_init_lua(
            r#"maki.setup({ provider = {
                allowed_models = { "anthropic/*", "openai/gpt-5" },
                excluded_models = { "*/*-preview" },
            } })"#
                .to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap()
        .expect("expected Some(RawConfig)");
    assert_eq!(
        raw.provider.allowed_models,
        Some(vec!["anthropic/*".into(), "openai/gpt-5".into()])
    );
    assert_eq!(
        raw.provider.excluded_models,
        Some(vec!["*/*-preview".into()])
    );
}

#[test]
fn setup_double_call_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .send_run_init_lua(
            "maki.setup({})\nmaki.setup({})".to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .expect_err("expected error for double setup");
    assert!(err.to_string().contains(ALREADY_CALLED_ERR), "got: {err}");
}

#[test]
fn setup_not_called_returns_none() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let raw = host
        .send_run_init_lua(
            "-- no setup call".to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap();
    assert!(raw.is_none());
}

#[test]
fn setup_all_sections_at_once() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let raw = host
        .send_run_init_lua(
            r#"maki.setup({
                always_yolo = true,
                always_fast = true,
                always_thinking = "adaptive",
                ui = { splash_animation = false, mouse_scroll_lines = 5 },
                agent = {
                    max_output_lines = 9000,
                    compaction_instructions = "Note plan.md",
                    post_compaction_instructions = "Re-read plan.md",
                },
                provider = { default_model = "anthropic/claude-opus-4-6" },
                storage = { max_log_files = 3 },
                plugins = { bash = { enabled = true, timeout_secs = 180 }, websearch = { enabled = false } },
            })"#
            .to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap()
        .expect("expected Some(RawConfig)");
    assert_eq!(raw.always_yolo, Some(true));
    assert_eq!(raw.always_fast, Some(true));
    assert_eq!(
        raw.always_thinking,
        Some(AlwaysThinking::Mode("adaptive".into()))
    );
    assert_eq!(raw.ui.splash_animation, Some(false));
    assert_eq!(raw.ui.mouse_scroll_lines, Some(5));
    assert_eq!(raw.agent.max_output_lines, Some(9000));
    assert_eq!(
        raw.agent.compaction_instructions.as_deref(),
        Some("Note plan.md")
    );
    assert_eq!(
        raw.agent.post_compaction_instructions.as_deref(),
        Some("Re-read plan.md")
    );
    assert_eq!(
        raw.provider.default_model.as_deref(),
        Some("anthropic/claude-opus-4-6")
    );
    assert_eq!(raw.storage.max_log_files, Some(3));
    assert_eq!(raw.plugins["bash"].enabled, Some(true));
    assert_eq!(
        raw.plugins["bash"].opts["timeout_secs"],
        serde_json::json!(180)
    );
    assert_eq!(raw.plugins["websearch"].enabled, Some(false));
}

const OPTS_PROBE_PLUGIN: &str = r#"
local opts = maki.api.register_options({
    timeout_secs = { default = 120, min = 5, desc = "Timeout." },
    label = { type = "string", desc = "Label." },
})
maki.api.register_tool({
    name = "opts_probe",
    description = "returns merged opts",
    schema = { type = "object", properties = {}, additionalProperties = false },
    audiences = { "main" },
    handler = function(input, ctx)
        return (maki.json.encode({
            timeout_secs = opts.timeout_secs,
            label = opts.label,
        }))
    end
})
"#;

const UNKNOWN_OPTION_ERR: &str =
    "unknown option \"typo\" for plugins.opts_plugin (valid options: label, timeout_secs)";
const OPTION_TYPE_ERR: &str =
    "invalid value for plugins.opts_plugin.timeout_secs: expected integer";
const OPTION_MIN_ERR: &str =
    "invalid value for plugins.opts_plugin.timeout_secs: 1 is below minimum (5)";
const OPTION_DESC_ERR: &str = "option \"timeout_secs\": desc is required";
const OPTION_NO_TYPE_ERR: &str = "option \"bare\": type is required when there is no default";
const OPTION_SPEC_KEY_ERR: &str = "option \"timeout_secs\": unknown spec key \"mins\"";
const OPTION_DEFAULT_TYPE_ERR: &str =
    "option \"timeout_secs\": default 120 does not match type string";
const OPTION_DEFAULT_MIN_ERR: &str = "option \"timeout_secs\": default 1 is below min (5)";
const OPTION_MIN_ON_STRING_ERR: &str = "option \"label\": min is not allowed for type string";
const OPTION_RESERVED_ERR: &str = "option \"enabled\": reserved name";
const OPTION_TWICE_ERR: &str = "register_options: called more than once";
const UNDECLARED_OPTS_ERR: &str = "unknown options in plugins.bare_plugin: timeout_secs \
(this plugin declares no options via maki.api.register_options)";

fn probe_opts(reg: &ToolRegistry) -> serde_json::Value {
    let out = exec_tool(reg, "opts_probe", serde_json::json!({})).unwrap();
    serde_json::from_str(&out).unwrap()
}

fn json_obj(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().expect("test opts must be an object").clone()
}

#[test_case::test_case(
    serde_json::json!({}),
    serde_json::json!(120), serde_json::Value::Null
    ; "defaults_without_user_opts"
)]
#[test_case::test_case(
    serde_json::json!({ "timeout_secs": 30, "label": "x" }),
    serde_json::json!(30), serde_json::json!("x")
    ; "user_opts_win"
)]
fn register_options_merges(
    opts: serde_json::Value,
    timeout_secs: serde_json::Value,
    label: serde_json::Value,
) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source_with_opts("opts_plugin", OPTS_PROBE_PLUGIN, json_obj(opts))
        .unwrap();

    let snap = probe_opts(&reg);
    assert_eq!(snap["timeout_secs"], timeout_secs);
    assert_eq!(snap["label"], label);
}

#[test_case::test_case(serde_json::json!({ "typo": 1 }), UNKNOWN_OPTION_ERR ; "unknown_key")]
#[test_case::test_case(serde_json::json!({ "timeout_secs": "abc" }), OPTION_TYPE_ERR ; "wrong_type")]
#[test_case::test_case(serde_json::json!({ "timeout_secs": 12.5 }), OPTION_TYPE_ERR ; "float_for_integer")]
#[test_case::test_case(serde_json::json!({ "timeout_secs": 1 }), OPTION_MIN_ERR ; "below_min")]
fn register_options_rejects_bad_user_opts(opts: serde_json::Value, expected: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_source_with_opts("opts_plugin", OPTS_PROBE_PLUGIN, json_obj(opts))
        .expect_err("plugin load should fail");
    assert!(err.to_string().contains(expected), "got: {err}");
}

#[test_case::test_case(
    r#"maki.api.register_options({ timeout_secs = { default = 120 } })"#,
    OPTION_DESC_ERR
    ; "missing_desc"
)]
#[test_case::test_case(
    r#"maki.api.register_options({ bare = { desc = "no type or default" } })"#,
    OPTION_NO_TYPE_ERR
    ; "missing_type_and_default"
)]
#[test_case::test_case(
    r#"maki.api.register_options({ timeout_secs = { default = 120, mins = 5, desc = "T." } })"#,
    OPTION_SPEC_KEY_ERR
    ; "unknown_spec_key"
)]
#[test_case::test_case(
    r#"maki.api.register_options({ timeout_secs = { type = "string", default = 120, desc = "T." } })"#,
    OPTION_DEFAULT_TYPE_ERR
    ; "default_contradicts_type"
)]
#[test_case::test_case(
    r#"maki.api.register_options({ timeout_secs = { default = 1, min = 5, desc = "T." } })"#,
    OPTION_DEFAULT_MIN_ERR
    ; "default_below_min"
)]
#[test_case::test_case(
    r#"maki.api.register_options({ label = { type = "string", min = 1, desc = "L." } })"#,
    OPTION_MIN_ON_STRING_ERR
    ; "min_on_string"
)]
#[test_case::test_case(
    r#"maki.api.register_options({ enabled = { default = true, desc = "E." } })"#,
    OPTION_RESERVED_ERR
    ; "reserved_enabled"
)]
#[test_case::test_case(
    r#"
    maki.api.register_options({ a = { default = 1, desc = "A." } })
    maki.api.register_options({ b = { default = 2, desc = "B." } })
    "#,
    OPTION_TWICE_ERR
    ; "called_twice"
)]
fn register_options_rejects_bad_spec(src: &str, expected: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_source("opts_plugin", src)
        .expect_err("plugin load should fail");
    assert!(err.to_string().contains(expected), "got: {err}");
}

#[test]
fn builtin_opts_flow_from_setup_plugins() {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let raw = host
        .send_run_init_lua(
            "maki.setup({ plugins = { grep = { search_result_limit = 42 } } })".to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap()
        .expect("expected Some(RawConfig)");
    host.load_builtins(&PluginsConfig::from_plugins(raw.plugins))
        .unwrap();

    let options = host.plugin_options().unwrap();
    let grep = options.get("grep").expect("grep options registered");
    let limit = grep
        .iter()
        .find(|o| o.name == "search_result_limit")
        .expect("search_result_limit declared");
    assert!(limit.default.is_some(), "declared default surfaces");
    assert!(limit.min.is_some(), "declared min surfaces");
    assert!(!limit.desc.is_empty(), "declared desc surfaces");
}

#[test_case::test_case(
    serde_json::json!({}),
    &["edit", "multiedit", "edit_lines"], &["insert_lines"]
    ; "defaults_on_insert_lines_opt_in"
)]
#[test_case::test_case(
    serde_json::json!({ "multiedit": false, "edit_lines": false, "insert_lines": true }),
    &["edit", "insert_lines"], &["multiedit", "edit_lines"]
    ; "toggles_flip_sub_tools"
)]
fn edit_sub_tools_follow_edit_opts(opts: serde_json::Value, on: &[&str], off: &[&str]) {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let config = PluginsConfig {
        enabled: true,
        names: vec!["edit".to_owned()],
        packages: Vec::new(),
        opts: HashMap::from([("edit".to_owned(), json_obj(opts))]),
    };
    host.load_builtins(&config).unwrap();
    for tool in on {
        assert!(reg.get(tool).is_some(), "{tool} should be registered");
    }
    for tool in off {
        assert!(reg.get(tool).is_none(), "{tool} should not be registered");
    }
}

/// Every bundled plugin with the edit sub-tools switched on, so the tool set
/// matches what a user who enabled everything would see.
fn whole_bundle() -> (Arc<ToolRegistry>, PluginHost) {
    let mut config = PluginsConfig::from_plugins(HashMap::new());
    config.opts.insert(
        "edit".to_owned(),
        EDIT_SUB_TOOLS
            .iter()
            .map(|name| (name.to_string(), serde_json::Value::Bool(true)))
            .collect(),
    );
    builtins_host_with(&config)
}

/// Pins `FILE_WRITE_TOOLS` to the actual `permission = "fs_write"`
/// declarations, so a new fs_write tool cannot quietly slip past the file
/// write policies keyed off that list (plan mode, cwd allow rules).
///
/// The other half of that guard is a registration rule: `permission =
/// "fs_write"` requires `mutable_path` (see `fs_write_without_mutable_path`),
/// which is what makes the dispatcher serialize the write and check for a
/// stale read.
#[test]
fn fs_write_tools_match_file_write_tools() {
    let (reg, _host) = whole_bundle();

    let snapshot = reg.iter();
    let mut declared: Vec<&str> = snapshot
        .iter()
        .filter(|t| t.tool.required_permission() == Some(Permission::FsWrite))
        .map(|t| t.name())
        .collect();
    declared.sort_unstable();
    let mut expected: Vec<&str> = FILE_WRITE_TOOLS.to_vec();
    expected.sort_unstable();

    assert_eq!(declared, expected, "{FILE_WRITE_TOOLS_DRIFT}");
}

/// `memory` pre-approves the file-write tools for the notes directory it owns,
/// and a rule can only name a registered tool. That turns `BUNDLED_PLUGINS`
/// order into load order: put `memory` above the plugins owning those tools
/// and its rules vanish with only a log line to show for it.
#[test]
fn builtins_load_in_an_order_that_keeps_every_plugin_rule() {
    let (_reg, host) = whole_bundle();

    let rules = host.plugin_rules().snapshot();
    let allowed: Vec<&str> = rules
        .iter()
        .filter(|rule| rule.effect == Effect::Allow)
        .filter_map(|rule| match &rule.tool {
            ToolKey::Native(name) => Some(name.as_ref()),
            _ => None,
        })
        .collect();
    let dropped: Vec<&&str> = FILE_WRITE_TOOLS
        .iter()
        .filter(|tool| !allowed.contains(tool))
        .collect();
    assert!(dropped.is_empty(), "{MEMORY_RULES_DROPPED}: {dropped:?}");
}

#[test]
fn undeclared_opts_fail_the_load() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_source_with_opts(
            "bare_plugin",
            "local x = 1",
            json_obj(serde_json::json!({ "timeout_secs": 30 })),
        )
        .expect_err("plugin load should fail");
    assert!(err.to_string().contains(UNDECLARED_OPTS_ERR), "got: {err}");
}

/// A disabled package keeps its options in `opts` but leaves `packages`, which
/// is exactly the shape `into_config` produces. Treating that as an unknown
/// name stopped maki from booting over options it was already ignoring, and
/// only for packages: a disabled builtin in the same state just warned.
#[test]
fn opts_for_a_disabled_package_do_not_stop_the_load() {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let config = PluginsConfig {
        enabled: true,
        names: vec!["grep".to_owned()],
        packages: Vec::new(),
        opts: HashMap::from([(
            "my_pack".to_owned(),
            json_obj(serde_json::json!({ "timeout_secs": 5 })),
        )]),
    };
    host.load_builtins(&config)
        .expect("a disabled package must not stop the builtins from loading");
    assert!(reg.get("grep").is_some(), "enabled plugin still loads");
}

#[test]
fn unknown_plugin_name_fails_load_builtins() {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let mut config = PluginsConfig::from_plugins(HashMap::new());
    config.names.push("gerp".to_string());
    let err = host
        .load_builtins(&config)
        .expect_err("load_builtins should fail");
    assert!(
        err.to_string().contains("no bundled plugin named \"gerp\""),
        "got: {err}"
    );
}

fn shadow_src() -> String {
    format!(
        r#"maki.api.register_tool({{
            name = "{SHADOWED_TOOL}",
            description = "{REPLACEMENT_DESC}",
            schema = {MINIMAL_SCHEMA},
            handler = function() return "replaced" end
        }})"#
    )
}

/// Turning a builtin off used to copy its name into `agent.disabled_tools`,
/// the name filter every request runs over the tool array, so a replacement
/// could load and still stay invisible to the model. That is why this walks
/// the whole path: init.lua, config, builtins, then the definitions a request
/// is built from.
#[test]
fn disabled_builtin_hands_its_tool_name_to_a_user_plugin() {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let raw = host
        .send_run_init_lua(
            format!("maki.setup({{ plugins = {{ {SHADOWED_TOOL} = {{ enabled = false }} }} }})"),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap()
        .expect("setup returns a config");
    let config = raw.into_config(&[]).unwrap();
    host.load_builtins(&config.plugins).unwrap();
    host.load_source(REPLACEMENT_PLUGIN, &shadow_src())
        .expect("a disabled builtin leaves its tool name free");

    let model = Model::from_spec("anthropic/claude-opus-4-8").unwrap();
    let filter = ToolFilter::from_config(&config.agent, &model, &[]);
    let ctx = DescriptionContext {
        filter: &filter,
        audience: ToolAudience::MAIN,
        workflow: false,
        mcp: false,
    };
    let defs = reg.definitions(&Vars::new(), &ctx, false);
    let shadowed = defs
        .as_array()
        .expect("definitions returns an array")
        .iter()
        .find(|def| def["name"] == SHADOWED_TOOL)
        .expect("the replacement must reach the model, not just `maki prompt --tools`");
    assert_eq!(shadowed["description"], REPLACEMENT_DESC);
}

#[test]
fn enabled_builtin_still_rejects_a_shadowing_plugin() {
    let (_reg, host) = builtins_host();
    let err = host
        .load_source(REPLACEMENT_PLUGIN, &shadow_src())
        .expect_err("an enabled builtin owns its tool name");
    assert!(
        matches!(err, PluginError::NameConflict { .. }),
        "got: {err}"
    );
}

#[test]
fn disabled_plugin_opts_are_ignored_not_rejected() {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let config = PluginsConfig {
        enabled: true,
        names: vec!["grep".to_owned()],
        packages: Vec::new(),
        opts: HashMap::from([(
            "bash".to_owned(),
            json_obj(serde_json::json!({ "timeout_secs": 180 })),
        )]),
    };
    host.load_builtins(&config).unwrap();
    assert!(reg.get("bash").is_none(), "bash stays disabled");
    assert!(reg.get("grep").is_some(), "enabled plugin still loads");
}

#[test_case::test_case("true", AlwaysThinking::Toggle(true) ; "bool")]
#[test_case::test_case("8192", AlwaysThinking::Budget(8192) ; "number")]
#[test_case::test_case("\"adaptive\"", AlwaysThinking::Mode("adaptive".into()) ; "string")]
fn setup_always_thinking_variants(lua_val: &str, expected: AlwaysThinking) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let raw = host
        .send_run_init_lua(
            format!("maki.setup({{ always_thinking = {lua_val} }})"),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap()
        .expect("expected Some(RawConfig)");
    assert_eq!(raw.always_thinking, Some(expected));
}

#[test]
fn setup_no_tool_registration_in_init_env() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .send_run_init_lua(
            r#"maki.register_tool({
                name = "sneaky",
                description = "should fail",
                audiences = { "main" },
                handler = function() return "nope" end
            })"#
            .to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .expect_err("register_tool should not be available in init.lua env");
    assert!(
        matches!(err, PluginError::Lua { .. }),
        "expected Lua error, got: {err}"
    );
}

#[test]
fn register_command_happy_path() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "cmd_plugin",
        r#"
        maki.api.register_command({
            name = "/hello",
            description = "says hello",
            handler = function(opts) end,
        })
        "#,
    )
    .unwrap();

    let reader = host.command_reader();
    let snap = reader.load();
    assert_eq!(snap.commands.len(), 1);
    assert_eq!(snap.commands[0].name.as_ref(), "/hello");
    assert_eq!(snap.commands[0].description.as_ref(), "says hello");
    assert_eq!(snap.commands[0].plugin.as_ref(), "cmd_plugin");
}

#[test_case::test_case("" => 0 ; "default_zero")]
#[test_case::test_case("nargs = 0," => 0 ; "zero")]
#[test_case::test_case("nargs = 1," => 1 ; "one")]
#[test_case::test_case(r#"nargs = "?","# => 1 ; "zero_or_one")]
#[test_case::test_case(r#"nargs = "*","# => usize::MAX ; "any")]
#[test_case::test_case(r#"nargs = "+","# => usize::MAX ; "one_or_more")]
fn register_command_nargs_values(nargs_field: &str) -> usize {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "cmd_nargs",
        &format!(
            r#"maki.api.register_command({{ name = "/test", {nargs_field} handler = function() end }})"#
        ),
    )
    .unwrap();

    host.command_reader().load().commands[0].max_args
}

#[test_case::test_case("a  b c", "a  b c|a,b,c" ; "raw_text_and_split_list")]
#[test_case::test_case("", "|" ; "empty_args")]
fn command_handler_receives_args_and_fargs(args: &str, expected_flash: &str) {
    let host = PluginHost::new(fresh_registry()).unwrap();
    host.load_source(
        "p",
        r#"
        maki.api.register_command({
            name = "/echo",
            nargs = "*",
            handler = function(opts)
                maki.ui.flash(opts.args .. "|" .. table.concat(opts.fargs, ","))
            end,
        })
        "#,
    )
    .unwrap();
    let rx = host.ui_action_rx();
    host.event_handle()
        .run_command(Arc::from("p"), Arc::from("/echo"), args.into(), 0);

    let action = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("command handler did not run");
    assert!(matches!(action, maki_lua::UiAction::Flash(msg) if msg == expected_flash));
}

const RUN_COMMAND_NO_ACTION: &str = "run_command did not reach the UI";

/// `/go` asks for `/cd ~/src` and flashes the `ok, err` pair it gets back. The
/// command line travels untouched, since the UI is the side that parses it, and
/// a handler reached at depth 0 asks for depth 1 so a chain of aliases keeps
/// counting toward the cap.
#[test_case::test_case(Ok(()), "true|nil" ; "dispatched")]
#[test_case::test_case(Err("unknown command".into()), "nil|unknown command" ; "rejected")]
fn run_command_round_trips_through_ui(reply: Result<(), String>, expected_flash: &str) {
    let host = PluginHost::new(fresh_registry()).unwrap();
    host.load_source(
        "p",
        r#"
        maki.api.register_command({
            name = "/go",
            handler = function()
                local ok, err = maki.api.run_command("/cd ~/src")
                maki.ui.flash(tostring(ok) .. "|" .. tostring(err))
            end,
        })
        "#,
    )
    .unwrap();
    let rx = host.ui_action_rx();
    host.event_handle()
        .run_command(Arc::from("p"), Arc::from("/go"), String::new(), 0);

    let maki_lua::UiAction::RunCommand {
        cmdline,
        depth,
        reply_tx,
    } = rx
        .recv_timeout(Duration::from_secs(5))
        .expect(RUN_COMMAND_NO_ACTION)
    else {
        panic!("{RUN_COMMAND_NO_ACTION}");
    };
    assert_eq!((cmdline.as_str(), depth), ("/cd ~/src", 1));
    reply_tx.send(reply).unwrap();

    let action = rx
        .recv_timeout(Duration::from_secs(5))
        .expect(RUN_COMMAND_NO_ACTION);
    assert!(matches!(action, maki_lua::UiAction::Flash(msg) if msg == expected_flash));
}
#[test_case::test_case(
    r#"maki.api.register_command({ name = "", handler = function() end })"#,
    "non-empty" ; "empty_name"
)]
#[test_case::test_case(
    r#"maki.api.register_command({ name = "/test", description = "no handler" })"#,
    "handler" ; "missing_handler"
)]
#[test_case::test_case(
    r#"maki.api.register_command({ name = "/test", nargs = -1, handler = function() end })"#,
    NARGS_ERR ; "negative_nargs"
)]
#[test_case::test_case(
    r#"maki.api.register_command({ name = "/test", nargs = 2, handler = function() end })"#,
    NARGS_ERR ; "nargs_two"
)]
#[test_case::test_case(
    r#"maki.api.register_command({ name = "/test", nargs = "!", handler = function() end })"#,
    NARGS_ERR ; "unknown_string_nargs"
)]
fn register_command_validation_rejects(src: &str, expected_err: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_source("bad_cmd", src)
        .expect_err("expected validation error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains(expected_err), "got: {err}");
}

#[test]
fn reload_replaces_commands() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "reload_cmd",
        r#"maki.api.register_command({ name = "/v1", handler = function() end })"#,
    )
    .unwrap();

    host.load_source(
        "reload_cmd",
        r#"maki.api.register_command({ name = "/v2", handler = function() end })"#,
    )
    .unwrap();
    let snap = host.command_reader().load();
    assert_eq!(snap.commands.len(), 1);
    assert_eq!(snap.commands[0].name.as_ref(), "/v2");
}

/// Builds a package directory with the given `plugin/*.lua` files.
fn package_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    let plugin_dir = tmp.path().join("plugin");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    for (name, source) in files {
        std::fs::write(plugin_dir.join(name), source).unwrap();
    }
    tmp
}

#[test]
fn package_loads_every_entrypoint_under_one_owner() {
    let pkg = package_dir(&[
        (
            "01_first.lua",
            r#"maki.api.register_command({ name = "/one", handler = function() end })"#,
        ),
        (
            "02_second.lua",
            r#"maki.api.register_command({ name = "/two", handler = function() end })"#,
        ),
    ]);

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_package(
        "demo",
        pkg.path(),
        maki_lua::PluginPermissions::trusted(),
        Default::default(),
    )
    .unwrap();

    let snap = host.command_reader().load();
    let mut names: Vec<&str> = snap.commands.iter().map(|c| c.name.as_ref()).collect();
    names.sort();
    assert_eq!(names, vec!["/one", "/two"]);
    assert!(
        snap.commands.iter().all(|c| c.plugin.as_ref() == "demo"),
        "every entrypoint must register under the package owner"
    );
}

/// One environment across the chunks, so an earlier file can set something up
/// for a later one. This is why the chunks are not separate loads.
#[test]
fn package_entrypoints_share_one_environment() {
    let pkg = package_dir(&[
        ("01_first.lua", "shared_value = 11\n"),
        (
            "02_second.lua",
            r#"
assert(shared_value == 11, "second chunk should see the first chunk's global")
maki.api.register_command({ name = "/ok", handler = function() end })
"#,
        ),
    ]);

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_package(
        "shared",
        pkg.path(),
        maki_lua::PluginPermissions::trusted(),
        Default::default(),
    )
    .unwrap();
    assert_eq!(host.command_reader().load().commands.len(), 1);
}

/// A package commits or it does not. `drop_plugin_keys` alone would leave the
/// keymap and the hint behind, so this is what proves the stronger unwind.
#[test]
fn package_failure_leaves_nothing_from_earlier_chunks() {
    let pkg = package_dir(&[
        (
            "01_first.lua",
            r#"
maki.api.register_command({ name = "/ghost", handler = function() end })
maki.keymap.set("n", "<C-g>", function() end, { desc = "ghost" })
maki.api.register_tool({
  name = "ghost_tool",
  description = "should not survive",
  schema = { type = "object", properties = {} },
  handler = function() return "x" end,
})
"#,
        ),
        ("02_second.lua", r#"error("boom")"#),
    ]);

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_package(
            "ghost",
            pkg.path(),
            maki_lua::PluginPermissions::trusted(),
            Default::default(),
        )
        .expect_err("a failing chunk must fail the whole package");
    assert!(err.to_string().contains("boom"), "got: {err}");

    assert_eq!(
        host.command_reader().load().commands.len(),
        0,
        "command from the first chunk survived a failed load"
    );
    assert_eq!(
        host.keymap_reader().load().entries.len(),
        0,
        "keymap from the first chunk survived a failed load"
    );
    assert!(
        !reg.has("ghost_tool"),
        "tool from the first chunk survived a failed load"
    );
}

#[test]
fn package_failure_discards_its_packadd_requests() {
    let site = site_with_two(
        (
            "broken_pack",
            "maki.packadd('lazy_pack')\nerror('stop this package')",
        ),
        (
            "lazy_pack",
            r#"maki.api.register_command({ name = "/lazy", handler = function() end })"#,
        ),
    );
    let found = maki_lua::discover(site.path());
    let (_, config) = discovered_config(&found);

    let host = PluginHost::new(fresh_registry()).unwrap();
    let failures = host.load_packages(&found.packages, &config);

    assert_eq!(failures.len(), 1, "got: {failures:?}");
    assert!(
        host.command_reader().load().commands.is_empty(),
        "a failed package must not activate another package"
    );
}

#[cfg(unix)]
#[test]
fn package_entrypoint_symlink_escape_blocked() {
    let pkg = package_dir(&[]);
    // Deliberately in a different directory tree, so the link really leaves
    // the package rather than pointing at a sibling inside it.
    let elsewhere = tempfile::TempDir::new().unwrap();
    let outside = elsewhere.path().join("outside.lua");
    std::fs::write(&outside, "return {}\n").unwrap();
    std::os::unix::fs::symlink(&outside, pkg.path().join("plugin").join("leak.lua")).unwrap();

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_package(
            "leaky",
            pkg.path(),
            maki_lua::PluginPermissions::trusted(),
            Default::default(),
        )
        .expect_err("an entrypoint linking out of the package must not load");
    assert!(
        matches!(err, PluginError::PackageEscape { .. }),
        "got: {err}"
    );
}

#[test]
fn package_without_entrypoints_errors() {
    let pkg = package_dir(&[]);
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_package(
            "empty",
            pkg.path(),
            maki_lua::PluginPermissions::trusted(),
            Default::default(),
        )
        .expect_err("a package with no entrypoint is a configuration error");
    assert!(
        matches!(err, PluginError::PackageEmpty { .. }),
        "got: {err}"
    );
}

#[test]
fn unreadable_entrypoint_directory_is_reported() {
    let pkg = tempfile::TempDir::new().unwrap();
    std::fs::write(pkg.path().join("plugin"), "not a directory").unwrap();
    let host = PluginHost::new(fresh_registry()).unwrap();

    let err = host
        .load_package(
            "unreadable",
            pkg.path(),
            maki_lua::PluginPermissions::trusted(),
            Default::default(),
        )
        .expect_err("an unreadable entrypoint directory must not look empty");

    assert!(matches!(err, PluginError::Io { .. }), "got: {err}");
}

/// Builds a site tree holding one package, the way a user cloning a repository
/// into the package directory would.
fn site_with_package(sub: &str, name: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("pack").join("vendor").join(sub).join(name);
    std::fs::create_dir_all(dir.join("plugin")).unwrap();
    for (file, source) in files {
        std::fs::write(dir.join("plugin").join(file), source).unwrap();
    }
    tmp
}

/// Permission decisions are keyed by tool name alone, so a package that takes
/// a name maki's builtin defaults are written for inherits those defaults, and
/// any "always allow" the user stored for the builtin. The load is allowed, the
/// user is told, once for the package however many names it took.
#[test_case::test_case(&[PERMISSION_KEYED_TOOL], 1 ; "permission_keyed_name_warns")]
#[test_case::test_case(&[PERMISSION_KEYED_TOOL, OTHER_PERMISSION_KEYED_TOOL], 1 ; "two_names_warn_once")]
#[test_case::test_case(&[PLAIN_TOOL], 0 ; "ordinary_name_is_quiet")]
fn package_taking_a_permission_keyed_tool_name_warns(tools: &[&str], expected: usize) {
    let source = tools
        .iter()
        .map(|tool| {
            format!(
                r#"maki.api.register_tool({{
            name = "{tool}",
            description = "{REPLACEMENT_DESC}",
            schema = {MINIMAL_SCHEMA},
            handler = function() return "" end
        }})"#
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let site = site_with_package("start", "perm_pack", &[("init.lua", &source)]);
    let found = maki_lua::discover(site.path());
    let (_, config) = discovered_config(&found);
    let host = PluginHost::new(fresh_registry()).unwrap();

    let warnings = host.load_packages(&found.packages, &config);

    assert_eq!(
        warnings
            .iter()
            .filter(|w| w.contains(PERMISSION_NAME_WARNING))
            .count(),
        expected,
        "got: {warnings:?}"
    );
}

/// The whole layer-1 path: find a package on disk, then load it.
#[test]
fn discovered_start_package_is_found_and_loaded() {
    let site = site_with_package(
        "start",
        "demo_pack",
        &[(
            "init.lua",
            r#"maki.api.register_command({ name = "/demo", handler = function() end })"#,
        )],
    );

    let found = maki_lua::discover(site.path());
    assert!(found.problems.is_empty(), "{:?}", found.problems);
    let names: Vec<String> = found.packages.iter().map(|p| p.name.clone()).collect();
    let config = PluginsConfig::from_plugins_and_packages(Default::default(), &names);

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    assert!(host.load_packages(&found.packages, &config).is_empty());

    let snap = host.command_reader().load();
    assert_eq!(snap.commands.len(), 1);
    assert_eq!(snap.commands[0].name.as_ref(), "/demo");
    assert_eq!(snap.commands[0].plugin.as_ref(), "demo_pack");
}

#[test]
fn custom_loader_runs_as_the_package_owner_with_spec_data() {
    let package = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(package.path().join("lua")).unwrap();
    std::fs::write(
        package.path().join("lua").join("entry.lua"),
        r#"
return {
  setup = function(command)
    maki.api.register_command({ name = command, handler = function() end })
    maki.api.register_tool({
      name = "custom_state",
      description = "Package state.",
      schema = { type = "object", properties = {} },
      audiences = { "main" },
      handler = function()
        return tostring(maki.pack.get({ "custom" })[1].active)
      end,
    })
  end,
}
"#,
    )
    .unwrap();

    let registry = fresh_registry();
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    host.send_global_init_lua(
        r#"
maki.pack.add({
  {
    src = "https://example.com/custom",
    name = "custom",
    data = { module = "entry", command = "/custom" },
  },
}, {
  load = function(package)
    require(package.spec.data.module).setup(package.spec.data.command)
  end,
})
"#
        .to_owned(),
        None,
    )
    .unwrap();
    let declared = host.declared_packages().unwrap();
    let packages = vec![maki_lua::DiscoveredPackage {
        name: "custom".to_owned(),
        dir: package.path().to_path_buf(),
        eager: true,
        requested: maki_lua::Requested::none(),
        origin: maki_lua::Origin::Fetched {
            src: "https://example.com/custom".to_owned(),
        },
        revision_guard: None,
    }];
    let config = PluginsConfig::from_plugins_and_packages(Default::default(), &["custom".into()]);

    let failures = host.load_declared_packages(&packages, &declared, &config);
    assert!(failures.is_empty(), "got: {failures:?}");

    let commands = host.command_reader().load();
    assert_eq!(commands.commands.len(), 1);
    assert_eq!(commands.commands[0].name.as_ref(), "/custom");
    assert_eq!(commands.commands[0].plugin.as_ref(), "custom");
    assert_eq!(
        exec_tool(&registry, "custom_state", serde_json::json!({})).unwrap(),
        "true"
    );
}

#[test]
fn managed_custom_loader_does_not_capture_a_manual_name_conflict() {
    let site = site_with_package(
        "start",
        "manual",
        &[(
            "init.lua",
            r#"maki.api.register_command({ name = "/manual", handler = function() end })"#,
        )],
    );
    let packages = maki_lua::discover(site.path()).packages;
    let host = PluginHost::new(fresh_registry()).unwrap();
    host.send_global_init_lua(
        r#"
maki.pack.add({
  { src = "https://example.com/manual", name = "manual" },
}, {
  load = function() error("managed custom loader ran") end,
})
"#
        .to_owned(),
        None,
    )
    .unwrap();
    let declared = host.declared_packages().unwrap();
    let config = PluginsConfig::from_plugins_and_packages(Default::default(), &["manual".into()]);

    let failures = host.load_declared_packages(&packages, &declared, &config);

    assert!(failures.is_empty(), "got: {failures:?}");
    let commands = host.command_reader().load();
    assert_eq!(commands.commands.len(), 1);
    assert_eq!(commands.commands[0].name.as_ref(), "/manual");
}

#[test]
fn loaded_revision_lock_protects_modules_read_after_startup() {
    const CURRENT: &str = "1111111111111111111111111111111111111111";
    const STALE: &str = "2222222222222222222222222222222222222222";

    let site = tempfile::TempDir::new().unwrap();
    let stale_dir = maki_pack::paths::revision_dir(site.path(), "late_pack", STALE);
    std::fs::create_dir_all(stale_dir.join("plugin")).unwrap();
    std::fs::create_dir_all(stale_dir.join("lua")).unwrap();
    std::fs::write(
        stale_dir.join("plugin").join("init.lua"),
        format!(
            r#"
maki.api.register_tool({{
  name = "late_pack",
  description = "Late module read.",
  schema = {MINIMAL_SCHEMA},
  audiences = {{ "main" }},
  handler = function() return require("late").value end,
}})
"#
        ),
    )
    .unwrap();
    std::fs::write(
        stale_dir.join("lua").join("late.lua"),
        "return { value = 'late ok' }\n",
    )
    .unwrap();
    std::fs::create_dir_all(maki_pack::paths::revision_dir(
        site.path(),
        "late_pack",
        CURRENT,
    ))
    .unwrap();
    let mut lockfile = maki_pack::lockfile::Lockfile::default();
    lockfile.record("late_pack", "https://example.com/late", CURRENT);
    let revision_guard = Arc::new(
        maki_pack::lock::Lock::acquire_shared(&maki_pack::paths::revision_lock(
            site.path(),
            "late_pack",
            STALE,
        ))
        .unwrap(),
    );
    let packages = vec![maki_lua::DiscoveredPackage {
        name: "late_pack".to_owned(),
        dir: stale_dir.clone(),
        eager: true,
        requested: maki_lua::Requested::none(),
        origin: maki_lua::Origin::Fetched {
            src: "https://example.com/late".to_owned(),
        },
        revision_guard: Some(revision_guard),
    }];
    let config =
        PluginsConfig::from_plugins_and_packages(Default::default(), &["late_pack".into()]);
    let registry = fresh_registry();
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();

    assert!(host.load_packages(&packages, &config).is_empty());
    drop(packages);
    let manager = maki_pack::manager::Manager::new(site.path());
    assert!(manager.prune(&lockfile).is_empty());
    assert!(stale_dir.is_dir(), "a loaded revision must not be pruned");
    assert_eq!(
        exec_tool(&registry, "late_pack", serde_json::json!({})).unwrap(),
        "late ok"
    );

    host.unload("late_pack").unwrap();
    assert!(manager.prune(&lockfile).is_empty());
    assert!(
        !stale_dir.exists(),
        "an unloaded stale revision can be pruned"
    );
}

/// Builtins must still load when a package is installed. Packages once shared
/// the builtin name list, which made `load_builtins` reject every one of them
/// by name and fail startup outright.
#[test]
fn installed_package_does_not_break_builtin_loading() {
    let site = site_with_package(
        "start",
        "demo_pack",
        &[(
            "init.lua",
            r#"maki.api.register_command({ name = "/demo", handler = function() end })"#,
        )],
    );

    let found = maki_lua::discover(site.path());
    let names: Vec<String> = found.packages.iter().map(|p| p.name.clone()).collect();
    let config = PluginsConfig::from_plugins_and_packages(Default::default(), &names);

    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_builtins(&config)
        .expect("an installed package must not stop the builtins from loading");
    assert!(host.load_packages(&found.packages, &config).is_empty());

    assert!(reg.has("grep"), "builtin tools should still be registered");
    let names: Vec<String> = host
        .command_reader()
        .load()
        .commands
        .iter()
        .map(|c| c.name.to_string())
        .collect();
    assert!(names.iter().any(|n| n == "/demo"), "got: {names:?}");
}

/// Options for an installed package must reach the package, not be rejected as
/// options for a plugin that does not exist.
#[test]
fn installed_package_may_take_options() {
    let site = site_with_package(
        "start",
        "opt_pack",
        &[(
            "init.lua",
            r#"
local opts = maki.api.register_options({
  depth = { type = "integer", desc = "Depth." },
})
if opts.depth == 3 then
  maki.api.register_command({ name = "/depth", handler = function() end })
end
"#,
        )],
    );
    let found = maki_lua::discover(site.path());

    let mut plugins: HashMap<String, maki_config::PluginFileConfig> = HashMap::new();
    let mut cfg = maki_config::PluginFileConfig::default();
    cfg.opts.insert("depth".to_owned(), serde_json::json!(3));
    plugins.insert("opt_pack".to_owned(), cfg);

    let config = PluginsConfig::from_plugins_and_packages(plugins, &["opt_pack".to_owned()]);

    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_builtins(&config)
        .expect("package options must not be rejected as unknown plugin options");
    assert!(host.load_packages(&found.packages, &config).is_empty());

    assert!(
        host.command_reader()
            .load()
            .commands
            .iter()
            .any(|command| command.name.as_ref() == "/depth")
    );
}

/// If `lua/` itself links out of the package, its target must not become the
/// sandbox root; otherwise everything under that target would be requireable.
#[cfg(unix)]
#[test]
fn symlinked_lua_directory_is_not_used_as_the_module_root() {
    let pkg = package_dir(&[("init.lua", r#"require("escaped")"#)]);

    let elsewhere = tempfile::TempDir::new().unwrap();
    std::fs::write(elsewhere.path().join("escaped.lua"), "return {}\n").unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), pkg.path().join("lua")).unwrap();

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_package(
            "linky",
            pkg.path(),
            maki_lua::PluginPermissions::trusted(),
            Default::default(),
        )
        .expect_err("a lua/ directory pointing out of the package must not resolve modules");
    assert!(err.to_string().contains("module not found"), "got: {err}");
}

/// An `opt/` package waits to be activated, so startup alone must not run it.
#[test]
fn discovered_opt_package_is_not_loaded_at_startup() {
    let site = site_with_package(
        "opt",
        "lazy_pack",
        &[(
            "init.lua",
            r#"maki.api.register_command({ name = "/lazy", handler = function() end })"#,
        )],
    );

    let found = maki_lua::discover(site.path());
    let names: Vec<String> = found.packages.iter().map(|p| p.name.clone()).collect();
    let config = PluginsConfig::from_plugins_and_packages(Default::default(), &names);

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    assert!(host.load_packages(&found.packages, &config).is_empty());

    assert_eq!(host.command_reader().load().commands.len(), 0);
}

/// Adds one `start` and one `opt` package to a site tree.
fn site_with_two(start: (&str, &str), opt: (&str, &str)) -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    for (sub, name, source) in [("start", start.0, start.1), ("opt", opt.0, opt.1)] {
        let dir = tmp.path().join("pack").join("vendor").join(sub).join(name);
        std::fs::create_dir_all(dir.join("plugin")).unwrap();
        std::fs::write(dir.join("plugin").join("init.lua"), source).unwrap();
    }
    tmp
}

fn discovered_config(found: &maki_lua::Discovery) -> (Vec<String>, PluginsConfig) {
    let names: Vec<String> = found.packages.iter().map(|p| p.name.clone()).collect();
    let config = PluginsConfig::from_plugins_and_packages(Default::default(), &names);
    (names, config)
}

/// The whole startup order: load what starts eagerly, then apply whatever
/// those loads recorded. `maki.packadd` only takes effect at the second step.
fn activate_all(
    host: &PluginHost,
    found: &maki_lua::Discovery,
    config: &PluginsConfig,
) -> Vec<String> {
    host.load_packages(&found.packages, config)
}

/// `maki.packadd` is the activation path for an `opt/` package. A `start`
/// package that calls it must get the named package loaded in the same
/// startup, not the next one, or its registrations never appear.
#[test]
fn packadd_from_a_start_package_activates_an_opt_package() {
    let site = site_with_two(
        ("waker_pack", r#"maki.packadd("lazy_pack")"#),
        (
            "lazy_pack",
            r#"maki.api.register_command({ name = "/lazy", handler = function() end })"#,
        ),
    );

    let found = maki_lua::discover(site.path());
    let (_, config) = discovered_config(&found);

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let failures = activate_all(&host, &found, &config);
    assert!(failures.is_empty(), "got: {failures:?}");

    let snap = host.command_reader().load();
    assert_eq!(
        snap.commands.len(),
        1,
        "the activated package must have registered its command"
    );
    assert_eq!(snap.commands[0].name.as_ref(), "/lazy");
}

/// `maki.packadd` is on the maki table for every plugin, but only the startup
/// drain reads what it records. A call after that drain would sit in the queue
/// for the rest of the session with no error and no log, so it is refused.
#[test]
fn packadd_after_startup_reports_rather_than_queueing() {
    let site = site_with_two(("waker_pack", ""), ("lazy_pack", ""));
    let found = maki_lua::discover(site.path());
    let (_, config) = discovered_config(&found);

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    assert!(
        host.load_packages(&found.packages, &config).is_empty(),
        "the start package must load"
    );

    let err = host
        .load_source("late_plugin", r#"maki.packadd("lazy_pack")"#)
        .expect_err("packadd must report once the startup drain is over");
    assert!(
        err.to_string().contains("already been loaded"),
        "got: {err}"
    );
}

/// A name that matches no installed package is reported. Doing nothing would
/// leave the user with a package that never loads and no reason why.
#[test]
fn packadd_reports_a_name_that_is_not_installed() {
    let site = site_with_two(
        ("waker_pack", r#"maki.packadd("absent_pack")"#),
        ("lazy_pack", ""),
    );

    let found = maki_lua::discover(site.path());
    let (_, config) = discovered_config(&found);

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let failures = activate_all(&host, &found, &config);
    assert_eq!(failures.len(), 1, "got: {failures:?}");
    assert!(failures[0].contains("absent_pack"), "got: {failures:?}");
}

/// A package the config disabled stays disabled. `packadd` must not be a way
/// around `plugins.<name>.enabled = false`.
#[test]
fn packadd_cannot_activate_a_disabled_package() {
    let site = site_with_two(
        ("waker_pack", r#"maki.packadd("lazy_pack")"#),
        (
            "lazy_pack",
            r#"maki.api.register_command({ name = "/lazy", handler = function() end })"#,
        ),
    );

    let found = maki_lua::discover(site.path());
    let names: Vec<String> = found.packages.iter().map(|p| p.name.clone()).collect();
    let mut plugins: HashMap<String, maki_config::PluginFileConfig> = HashMap::new();
    plugins.insert(
        "lazy_pack".to_owned(),
        maki_config::PluginFileConfig {
            enabled: Some(false),
            ..Default::default()
        },
    );
    let config = PluginsConfig::from_plugins_and_packages(plugins, &names);

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let failures = activate_all(&host, &found, &config);
    assert_eq!(failures.len(), 1, "got: {failures:?}");
    assert_eq!(
        host.command_reader().load().commands.len(),
        0,
        "a disabled package must not register anything"
    );
}

/// A package that asks for nothing gets nothing. Without a `plugin.toml` the
/// guarded APIs must refuse, so a downloaded package cannot reach the network
/// or the environment just by being installed.
#[test]
fn package_without_manifest_cannot_use_guarded_apis() {
    let site = site_with_package(
        "start",
        "greedy_pack",
        &[(
            "init.lua",
            r#"
local ok = pcall(function() return maki.env.config_dir() end)
maki.api.register_command({
  name = ok and "/allowed" or "/denied",
  handler = function() end,
})
"#,
        )],
    );

    let found = maki_lua::discover(site.path());
    let names: Vec<String> = found.packages.iter().map(|p| p.name.clone()).collect();
    let config = PluginsConfig::from_plugins_and_packages(Default::default(), &names);

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    assert!(host.load_packages(&found.packages, &config).is_empty());

    let snap = host.command_reader().load();
    assert_eq!(snap.commands.len(), 1);
    assert_eq!(
        snap.commands[0].name.as_ref(),
        "/denied",
        "a package requesting nothing must not reach maki.env"
    );
}

/// The manifest is what a manual install is granted, so a package that asks
/// for `fs_read` gets it without any further approval.
#[test]
fn manual_package_is_granted_what_its_manifest_requests() {
    let site = site_with_package(
        "start",
        "asking_pack",
        &[(
            "init.lua",
            r#"
local ok = pcall(function() return maki.env.config_dir() end)
maki.api.register_command({
  name = ok and "/allowed" or "/denied",
  handler = function() end,
})
"#,
        )],
    );
    let pkg_dir = site
        .path()
        .join("pack")
        .join("vendor")
        .join("start")
        .join("asking_pack");
    std::fs::write(
        pkg_dir.join("plugin.toml"),
        "[permissions]\nfs_read = true\n",
    )
    .unwrap();

    let found = maki_lua::discover(site.path());
    let names: Vec<String> = found.packages.iter().map(|p| p.name.clone()).collect();
    let config = PluginsConfig::from_plugins_and_packages(Default::default(), &names);

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    assert!(host.load_packages(&found.packages, &config).is_empty());

    let snap = host.command_reader().load();
    assert_eq!(snap.commands[0].name.as_ref(), "/allowed");
}

/// The manifest body and the fragment the warning has to name.
fn floor_above_running_version() -> (String, String) {
    let running = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
    let required = format!("{}.0.0", running.major + 1);
    (format!("min_maki_version = {required:?}\n"), required)
}

fn malformed_floor() -> (String, String) {
    (MALFORMED_FLOOR.to_owned(), FLOORED_PACKAGE.to_owned())
}

/// The version floor covers installed packages, not just `init.lua`: a package
/// asking for a newer Maki is skipped with a warning, registers nothing, and
/// leaves the packages loading beside it alone.
#[test_case::test_case(floor_above_running_version ; "required version is newer")]
#[test_case::test_case(malformed_floor ; "required version is not a string")]
fn incompatible_package_is_skipped_and_its_sibling_still_loads(floor: fn() -> (String, String)) {
    let (manifest, expected) = floor();
    let site = site_with_package(
        "start",
        FLOORED_PACKAGE,
        &[(
            "init.lua",
            r#"
maki.api.register_command({ name = "/future", handler = function() end })
maki.api.register_tool({
  name = "future_tool",
  description = "must never register",
  schema = { type = "object", properties = {} },
  handler = function() return "x" end,
})
"#,
        )],
    );
    let start = site.path().join("pack").join("vendor").join("start");
    std::fs::write(start.join(FLOORED_PACKAGE).join("plugin.toml"), manifest).unwrap();
    let sibling = start.join(SIBLING_PACKAGE).join("plugin");
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(
        sibling.join("init.lua"),
        r#"maki.api.register_command({ name = "/sibling", handler = function() end })"#,
    )
    .unwrap();

    let found = maki_lua::discover(site.path());
    let (_, config) = discovered_config(&found);
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let failures = host.load_packages(&found.packages, &config);

    let warning = failures
        .iter()
        .find(|w| w.contains(SKIPPED_PLUGIN_WARNING))
        .unwrap_or_else(|| panic!("no skip warning in {failures:?}"));
    assert!(warning.contains(&expected), "{warning}");
    assert_eq!(failures.len(), 1, "got: {failures:?}");
    assert!(!reg.has("future_tool"));

    let snap = host.command_reader().load();
    let names: Vec<&str> = snap.commands.iter().map(|c| c.name.as_ref()).collect();
    assert_eq!(names, vec!["/sibling"]);
}

#[test]
fn unload_clears_commands() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "cmd_only",
        r#"maki.api.register_command({ name = "/bye", handler = function() end })"#,
    )
    .unwrap();
    assert_eq!(host.command_reader().load().commands.len(), 1);

    host.unload("cmd_only").unwrap();
    assert_eq!(host.command_reader().load().commands.len(), 0);
}

/// `/tasks` and `/sessions` used to be Rust commands. The plugins that took
/// them over have to keep the names, or the palette quietly loses a row.
#[test]
fn builtin_plugins_register_their_commands() {
    let (_reg, host) = builtins_host();
    let snap = host.command_reader().load();
    let names: Vec<&str> = snap.commands.iter().map(|c| c.name.as_ref()).collect();
    for command in BUILTIN_COMMANDS {
        assert!(names.contains(command), "missing {command} in {names:?}");
    }
}

#[test]
fn job_callback_finishes_after_handler_returns_nil() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "job_after_return",
            description = "on_exit finishes after handler returns nil",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                maki.fn.jobstart("true", {{
                    on_exit = function(_, code)
                        ctx:finish("exit=" .. tostring(code))
                    end,
                }})
                return nil
            end
        }})"#,
    );
    host.load_source("job_after_return", &src).unwrap();
    let out = exec_tool(&reg, "job_after_return", serde_json::json!({})).unwrap();
    assert_eq!(out, "exit=0");
}

#[test]
fn ctx_set_deadline_times_out() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "deadline_test",
            description = "uses ctx:set_deadline",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                ctx:set_deadline(2)
                maki.fn.jobstart("sleep 30", {{
                    on_exit = function(_, _) ctx:finish("should-not-reach") end,
                }})
                return nil
            end
        }})"#,
    );
    host.load_source("deadline_test", &src).unwrap();
    let err = exec_tool(&reg, "deadline_test", serde_json::json!({})).unwrap_err();
    assert!(err.contains(TIMED_OUT_SUBSTR), "got: {err}");
}

#[test]
fn ctx_set_deadline_twice_errors() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "deadline_twice",
            description = "calls set_deadline twice",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                ctx:set_deadline(5)
                ctx:set_deadline(5)
            end
        }})"#,
    );
    host.load_source("deadline_twice", &src).unwrap();
    let err = exec_tool(&reg, "deadline_twice", serde_json::json!({})).unwrap_err();
    assert!(err.contains(DEADLINE_ALREADY_SET_ERR), "got: {err}");
}

/// Generous: every wait below ends on an event, never on the clock, so only
/// an already failing test pays this.
const CANCEL_TEST_TIMEOUT: Duration = Duration::from_secs(30);
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(10);

fn poll_until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + CANCEL_TEST_TIMEOUT;
    loop {
        if let Some(got) = check() {
            return got;
        }
        assert!(std::time::Instant::now() < deadline, "{what}");
        std::thread::sleep(CANCEL_POLL_INTERVAL);
    }
}

const PARKED_DEADLINE_REPLY: &str = "partial: timeout";
const PARKED_DEADLINE_PLUGIN: &str = r#"
maki.api.register_tool({
    name = "parked_deadline",
    description = "parks past its deadline, finishing from its cancel hook",
    schema = { type = "object", properties = {}, additionalProperties = false },
    audiences = { "main" },
    handler = function(input, ctx)
        ctx:set_deadline(1)
        maki.async.on_cancel(function(reason)
            ctx:finish({ llm_output = "partial: " .. reason, is_error = true })
        end)
        maki.fn.jobwait(maki.fn.jobstart("sleep 30"))
        return "unreachable"
    end,
})
"#;

/// A handler parked in an await runs no Lua when its deadline lapses, so the
/// host is what ends it, by raising inside the await. Its cancel hooks still
/// get that last slice, and the reply they finish with beats the generic
/// timeout error. The handler that already returned nil takes a different
/// road out, unit tested in `runtime.rs`.
#[test]
fn parked_handler_reports_its_hook_finish_reply_on_deadline() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("parked_deadline_plugin", PARKED_DEADLINE_PLUGIN)
        .unwrap();

    let result = exec_tool(&reg, "parked_deadline", json!({}));

    assert_eq!(result, Err(PARKED_DEADLINE_REPLY.to_owned()));
    drop(host);
}

const BASH_CANCEL_ID: &str = "bash-cancel-1";
/// Mirrors the cancelled marker in `plugins/lib/maki/partial.lua`.
const BASH_PARTIAL_MARKER: &str = "[cancelled by user; output above is partial]";
/// Assembled by printf so the probe never appears in the command header.
const BASH_PARTIAL_PROBE: &str = "XY";
const BASH_PARTIAL_CMD: &str = "printf '%s%s\\n' X Y && sleep 30";

/// Esc mid-stream on a real bash run: the lines printed so far come back as
/// an error reply ending in the marker, not a bare "cancelled".
#[test]
fn cancelled_bash_keeps_streamed_output_as_partial() {
    let (tx, events) = flume::unbounded();
    let event_tx = maki_agent::EventSender::new(tx, 0);
    let (trigger, token) = maki_agent::CancelToken::new();
    let (result_tx, result_rx) = flume::bounded(1);
    std::thread::spawn(move || {
        let (reg, host) = builtins_host();
        let mut ctx = maki_agent::tools::test_support::stub_ctx_with(
            &maki_agent::AgentMode::Build,
            Some(&event_tx),
            Some(BASH_CANCEL_ID),
        );
        ctx.cancel = token;
        // The rtk probe costs up to two 2s job waits before the command even
        // starts: pointless here, and a flake risk under load.
        ctx.config.rtk = false;
        let input = json!({ "command": BASH_PARTIAL_CMD });
        result_tx
            .send(exec_with_ctx(&reg, "bash", input, &ctx))
            .ok();
        drop(host);
    });

    let buf = poll_until("bash must publish its live buf", || {
        recv_live_buf(&events, BASH_CANCEL_ID)
    });
    poll_until("bash output never reached the live buf", || {
        buf.take().text().contains(BASH_PARTIAL_PROBE).then_some(())
    });

    trigger.cancel();

    let err = result_rx
        .recv_timeout(CANCEL_TEST_TIMEOUT)
        .expect("cancelled bash must settle")
        .expect_err("a partial reply is an error reply");
    assert_eq!(err, format!("{BASH_PARTIAL_PROBE}\n{BASH_PARTIAL_MARKER}"));
}

#[test]
fn restore_tool_async_ordering_and_delivery() {
    let (_reg, host) = builtins_host();

    let input = serde_json::json!({"command": "echo ok", "timeout": 1});

    let handle = host.event_handle();
    let (tx, rx) = flume::unbounded();
    let event_tx = maki_agent::EventSender::new(tx, 0);

    let bash_item = |id: &str| maki_lua::RestoreItem {
        tool: Arc::from("bash"),
        tool_use_id: id.to_owned(),
        output: "tool bash timed out after 1s".to_owned(),
        input: input.clone(),
        is_error: true,
        tool_output_lines: ToolOutputLines::default(),
        theme_gen: None,
        clicks: Vec::new(),
        state: None,
    };
    let unknown_item = maki_lua::RestoreItem {
        tool: Arc::from("definitely_not_a_tool"),
        tool_use_id: "unknown_id".to_owned(),
        output: "ignored".to_owned(),
        input: serde_json::json!({}),
        is_error: false,
        tool_output_lines: ToolOutputLines::default(),
        theme_gen: None,
        clicks: Vec::new(),
        state: None,
    };

    handle.request_restore(unknown_item, event_tx.clone());
    handle.request_restore(bash_item("a"), event_tx.clone());
    handle.request_restore(bash_item("b"), event_tx.clone());

    handle.wait_restore_complete_for_test();

    let snapshots: Vec<maki_agent::Envelope> = rx.drain().collect();

    let tool_ids: Vec<&str> = snapshots
        .iter()
        .filter_map(|env| match &env.event {
            maki_agent::AgentEvent::ToolSnapshot { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();

    assert!(
        !tool_ids.contains(&"unknown_id"),
        "unknown tool should emit no snapshots"
    );
    assert!(
        tool_ids.contains(&"a"),
        "known tool 'a' should emit snapshot"
    );
    assert!(
        tool_ids.contains(&"b"),
        "known tool 'b' should emit snapshot"
    );
}

#[test_case::test_case(
    "write",
    serde_json::json!({"path": "/tmp/x.md", "content": "alpha\nbeta"}),
    "wrote 10 bytes to /tmp/x.md",
    &["alpha", "beta"]
    ; "write_tool_restores_file_content"
)]
#[test_case::test_case(
    "memory",
    serde_json::json!({"command": "write", "path": "n.md", "content": "gamma"}),
    "wrote n.md (1 lines)",
    &["gamma"]
    ; "memory_write_restores_saved_content"
)]
fn restore_rebuilds_body_from_input_content(
    tool: &str,
    input: serde_json::Value,
    summary: &str,
    expected: &[&str],
) {
    let (_reg, host) = builtins_host();
    let handle = host.event_handle();
    let (tx, rx) = flume::unbounded();

    handle.request_restore(
        maki_lua::RestoreItem {
            tool: Arc::from(tool),
            tool_use_id: "restore_id".to_owned(),
            output: summary.to_owned(),
            input,
            is_error: false,
            tool_output_lines: ToolOutputLines::default(),
            theme_gen: None,
            clicks: vec![0],
            state: None,
        },
        maki_agent::EventSender::new(tx, 0),
    );
    handle.wait_restore_complete_for_test();

    let mut text = String::new();
    for env in rx.drain() {
        if let maki_agent::AgentEvent::ToolSnapshot { snapshot, .. } = env.event {
            for line in snapshot.lines.iter() {
                for span in &line.spans {
                    text.push_str(&span.text);
                }
            }
        }
    }

    for needle in expected {
        assert!(
            text.contains(needle),
            "restored body missing '{needle}', got: {text}"
        );
    }
    assert!(
        !text.contains(summary),
        "restored body should show content, not the summary: {text}"
    );
}

/// Guards the stale-cancelled-handle bug: `permission_scopes` must call
/// the plugin callback and return parsed scopes, not fall back to raw JSON.
/// A leaked `{"command":...}` scope would break allow rules.
#[test_case::test_case("git status" ; "parseable command")]
#[test_case::test_case("echo 'unterminated" ; "unparseable command")]
fn bash_permission_scopes_never_falls_back_to_json(command: &str) {
    let (reg, _host) = builtins_host();

    let input = serde_json::json!({ "command": command });
    let entry = reg.get("bash").expect("bash registered");
    let inv = entry.tool.parse(&input).expect("parse failed");
    let scopes = smol::block_on(inv.permission_scopes())
        .expect("permission_scopes returned None (would fall back to raw JSON)");

    assert!(
        !scopes.scopes.iter().any(|s| s.contains("\"command\"")),
        "fell back to raw JSON scope: {:?}",
        scopes.scopes
    );
}

/// Every command in a chain needs its own scope, otherwise one allow rule
/// covers commands nobody approved. The redirect case is the one that used to
/// slip: tree-sitter hangs a trailing `2>&1` off the whole chain, so the chain
/// arrived as a single scope starting with `cd `, and a `cd *` rule took it.
#[test_case::test_case(
    "cd /tmp && cargo check 2>&1 | tail -3",
    &["cd /tmp", "cargo check 2>&1", "tail -3"]
    ; "chain_with_redirect_and_pipe"
)]
#[test_case::test_case(
    "ls\n# a note\npwd",
    &["ls", "pwd"]
    ; "comments_are_not_scopes"
)]
#[test_case::test_case(
    "if [ -f x ]; then rm x; fi",
    &["if [ -f x ]; then rm x; fi"]
    ; "block_stays_one_scope"
)]
#[test_case::test_case(
    "cd /tmp && > log",
    &["cd /tmp", "> log"]
    ; "bodiless_redirect_is_its_own_scope"
)]
fn bash_permission_scopes_split_per_command(command: &str, expected: &[&str]) {
    let (reg, _host) = builtins_host();

    let input = serde_json::json!({ "command": command });
    let entry = reg.get("bash").expect("bash registered");
    let inv = entry.tool.parse(&input).expect("parse failed");
    let scopes = smol::block_on(inv.permission_scopes()).expect("permission_scopes returned None");

    assert!(!scopes.force_prompt, "command: {command}");
    assert_eq!(scopes.scopes, expected, "command: {command}");
}

fn exec_tool_with_perms(
    perms: maki_lua::PluginPermissions,
    src: &str,
    tool: &str,
    input: serde_json::Value,
) -> Result<String, String> {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source_with_permissions("perm_test", src, perms)
        .unwrap();
    exec_tool(&reg, tool, input)
}

fn perm_tool_src(name: &str, handler_body: &str) -> String {
    format!(
        r#"maki.api.register_tool({{
            name = "{name}",
            description = "d",
            schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
            handler = function(input, ctx)
                {handler_body}
            end,
        }})"#
    )
}

#[test_case::test_case(
    "read_deny",
    r#"local ok, err = pcall(function() maki.fs.read("/etc/hostname") end)
                return tostring(err)"#,
    "fs_read"
    ; "fs_read_denied"
)]
#[test_case::test_case(
    "write_deny",
    r#"local ok, err = pcall(function() maki.fs.write("/tmp/test", "x") end)
                return tostring(err)"#,
    "fs_write"
    ; "fs_write_denied"
)]
#[test_case::test_case(
    "run_deny",
    r#"local ok, err = pcall(function() maki.fn.jobstart("echo hi") end)
                return tostring(err)"#,
    "run"
    ; "run_denied"
)]
fn denied_permission_blocks_api(tool_name: &str, handler_body: &str, expected_perm: &str) {
    let src = perm_tool_src(tool_name, handler_body);
    let result = exec_tool_with_perms(
        maki_lua::PluginPermissions::denied(),
        &src,
        tool_name,
        serde_json::json!({}),
    )
    .unwrap();
    assert!(result.contains(PERMISSION_DENIED_MSG), "got: {result}");
    assert!(result.contains(expected_perm), "got: {result}");
}

#[test]
fn user_plugin_with_fs_read_can_read_but_not_write() {
    let src = perm_tool_src(
        "rw_test",
        r#"local read_ok = pcall(function() maki.fs.read("/dev/null") end)
                local write_ok = pcall(function() maki.fs.write("/tmp/test", "x") end)
                return "read=" .. tostring(read_ok) .. ",write=" .. tostring(write_ok)"#,
    );
    let mut perms = maki_lua::PluginPermissions::denied();
    perms.set(maki_lua::Permission::FsRead, true);
    let result = exec_tool_with_perms(perms, &src, "rw_test", serde_json::json!({})).unwrap();
    assert!(result.contains("read=true"), "got: {result}");
    assert!(result.contains("write=false"), "got: {result}");
}

/// Locating maki's own directories, or a program on `$PATH`, answers where a
/// file lives and never what the environment holds. `fs_read` is what these
/// cost, and it is also what they need, so `env` stays the key to the process
/// environment alone.
#[test_case::test_case("maki.env.state_dir()" ; "state_dir")]
#[test_case::test_case(r#"maki.fn.executable("ls")"# ; "executable")]
fn location_queries_cost_fs_read(call: &str) {
    const TOOL: &str = "location_test";
    let src = perm_tool_src(
        TOOL,
        &format!(
            r#"local ok, err = pcall(function() {call} end)
                return tostring(ok) .. ":" .. tostring(err)"#
        ),
    );

    let mut fs_read = maki_lua::PluginPermissions::denied();
    fs_read.set(maki_lua::Permission::FsRead, true);
    let granted = exec_tool_with_perms(fs_read, &src, TOOL, serde_json::json!({})).unwrap();
    assert!(granted.starts_with("true"), "got: {granted}");

    let refused = exec_tool_with_perms(
        maki_lua::PluginPermissions::denied(),
        &src,
        TOOL,
        serde_json::json!({}),
    )
    .unwrap();
    assert!(refused.contains(PERMISSION_DENIED_MSG), "got: {refused}");
    assert!(refused.contains("fs_read"), "got: {refused}");
}

const PATH_FIELD_SCHEMA: &str = r#"{
    type = "object",
    properties = { path = { type = "string" } },
    required = { "path" },
}"#;

#[test_case::test_case(STRING_FIELD_SCHEMA, "nonexistent" ; "missing_field")]
#[test_case::test_case(NON_STRING_FIELD_SCHEMA, "count" ; "non_string_field")]
fn mutable_path_invalid_rejected(schema: &str, scope_field: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "bad_mpath",
            description = "test",
            schema = {schema},
            mutable_path = "{scope_field}",
            handler = function() return "" end
        }})"#,
    );
    let err = host
        .load_source("bad_mpath_plugin", &src)
        .expect_err("expected error for invalid mutable_path");

    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(
        err.to_string().contains("mutable_path")
            && err.to_string().contains(INVALID_PERMISSION_SCOPE_ERR),
        "got: {err}"
    );
}

#[test]
fn mutable_path_returns_path_from_input() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "mp_read",
            description = "test",
            schema = {PATH_FIELD_SCHEMA},
            mutable_path = "path",
            handler = function() return "" end
        }})"#,
    );
    host.load_source("mp_read_plugin", &src).unwrap();

    let entry = reg.get("mp_read").expect("tool not registered");
    let inv = entry
        .tool
        .parse(&serde_json::json!({ "path": "/tmp/foo.txt" }))
        .expect("parse failed");
    assert_eq!(inv.mutable_path(), Some(Path::new("/tmp/foo.txt")));
}

#[test]
fn pure_functions_not_guarded() {
    let src = perm_tool_src(
        "pure_test",
        r#"local dirname_ok = pcall(function() maki.fs.dirname("/foo/bar") end)
                local basename_ok = pcall(function() maki.fs.basename("/foo/bar") end)
                local json_ok = pcall(function() maki.json.encode({a=1}) end)
                return "dirname=" .. tostring(dirname_ok) .. ",basename=" .. tostring(basename_ok) .. ",json=" .. tostring(json_ok)"#,
    );
    let result = exec_tool_with_perms(
        maki_lua::PluginPermissions::denied(),
        &src,
        "pure_test",
        serde_json::json!({}),
    )
    .unwrap();
    assert!(result.contains("dirname=true"), "got: {result}");
    assert!(result.contains("basename=true"), "got: {result}");
    assert!(result.contains("json=true"), "got: {result}");
}

#[test]
fn runaway_allocation_hits_memory_limit_instead_of_oom() {
    const LIMITED: &str = "limited";
    let src = r#"
        local ok, err = pcall(function()
            local t = {}
            local chunk = string.rep("x", 1024 * 1024)
            while true do
                t[#t + 1] = chunk .. tostring(#t)
            end
        end)
        if ok then error("expected allocation to fail under the memory limit") end
        if not string.find(tostring(err), "memory") then
            error("expected an out-of-memory error, got: " .. tostring(err))
        end
    "#;
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(LIMITED, src)
        .expect("plugin should hit the memory limit and recover, not crash the process");
}

#[test]
fn start_hook_publishes_live_buf_for_tool_use_id() {
    let (reg, _host) = start_hook_fixture();
    let rx = run_start(&reg, "st_tool", serde_json::json!({"code": "line1\nline2"}));
    let body = recv_live_buf(&rx, START_TOOL_USE_ID).expect("start must publish a LiveToolBuf");
    let text = body.take().text();
    assert!(text.contains("line1"), "preview must render input: {text}");
}

#[test]
fn start_hook_error_does_not_fail_tool() {
    let (reg, _host) = start_hook_fixture();
    let _rx = run_start(&reg, "st_boom", serde_json::json!({"code": "x"}));
    let out = exec_tool(&reg, "st_boom", serde_json::json!({"code": "x"})).expect("handler ok");
    assert_eq!(out, "handled");
}

#[test]
fn start_skipped_for_tool_without_start_fn() {
    let (reg, _host) = start_hook_fixture();
    let rx = run_start(&reg, "st_plain", serde_json::json!({"code": "x"}));
    assert!(
        recv_live_buf(&rx, START_TOOL_USE_ID).is_none(),
        "no start fn must mean no preview"
    );
}

/// `start` runs before permission checks, so its ctx can read and preview
/// but dispatch/finish/deadline must come back as `(nil, err)`.
#[test]
fn start_ctx_capabilities() {
    let (reg, _host) = start_hook_fixture();
    let rx = run_start(&reg, "st_probe", serde_json::json!({"code": "x"}));
    let body = recv_live_buf(&rx, START_TOOL_USE_ID).expect("probe publishes a buf");
    let text = body.take().text();
    assert_eq!(
        text,
        "call_tool_err finish_err deadline_err config_ok cancelled_ok workflow_ok audience_ok tol_ok",
        "start ctx capability matrix mismatch"
    );
}

const START_TOOL_USE_ID: &str = "start-tu-1";

fn start_hook_fixture() -> (Arc<ToolRegistry>, PluginHost) {
    let src = format!(
        r#"
local function preview(input, ctx)
    local buf = maki.ui.buf()
    buf:set_lines({{ input.code }})
    ctx:live_buf(buf)
end
maki.api.register_tool({{
    name = "st_tool",
    description = "test",
    schema = {CODE_SCHEMA},
    start = preview,
    handler = function(input, ctx) return "handled" end,
}})
maki.api.register_tool({{
    name = "st_boom",
    description = "test",
    schema = {CODE_SCHEMA},
    start = function(input, ctx) error("boom") end,
    handler = function(input, ctx) return "handled" end,
}})
maki.api.register_tool({{
    name = "st_plain",
    description = "test",
    schema = {CODE_SCHEMA},
    handler = function(input, ctx) return "handled" end,
}})
maki.api.register_tool({{
    name = "st_probe",
    description = "test",
    schema = {CODE_SCHEMA},
    start = function(input, ctx)
        local parts = {{}}
        local function pair_err(v, e)
            return v == nil and type(e) == "string"
        end
        parts[1] = pair_err(maki.agent.call_tool(ctx, "st_plain", {{ code = "x" }})) and "call_tool_err"
            or "call_tool_ok"
        parts[2] = pair_err(ctx:finish("x")) and "finish_err" or "finish_ok"
        parts[3] = pair_err(ctx:set_deadline(5)) and "deadline_err" or "deadline_ok"
        parts[4] = type(ctx:config()) == "table" and "config_ok" or "config_bad"
        parts[5] = ctx:cancelled() == false and "cancelled_ok" or "cancelled_bad"
        parts[6] = type(ctx:workflow()) == "boolean" and "workflow_ok" or "workflow_bad"
        parts[7] = type(ctx:audience()) == "string" and "audience_ok" or "audience_bad"
        parts[8] = type(ctx:tool_output_lines()) == "table" and "tol_ok" or "tol_bad"
        local buf = maki.ui.buf()
        buf:set_lines({{ table.concat(parts, " ") }})
        ctx:live_buf(buf)
    end,
    handler = function(input, ctx) return "handled" end,
}})
"#
    );
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("start_hooks", &src).unwrap();
    (reg, host)
}

/// `start` is awaited to completion, so the returned receiver already holds
/// everything the hook emitted.
fn run_start(
    reg: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
) -> flume::Receiver<maki_agent::Envelope> {
    let (tx, rx) = flume::unbounded::<maki_agent::Envelope>();
    let event_tx = maki_agent::EventSender::new(tx, 0);
    let ctx = maki_agent::tools::test_support::stub_ctx_with(
        &maki_agent::AgentMode::Build,
        Some(&event_tx),
        Some(START_TOOL_USE_ID),
    );
    let inv = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"))
        .tool
        .parse(&input)
        .expect("parse failed");
    smol::block_on(inv.start(&ctx));
    rx
}

fn recv_live_buf(
    rx: &flume::Receiver<maki_agent::Envelope>,
    id: &str,
) -> Option<Arc<maki_agent::SharedBuf>> {
    rx.drain().find_map(|env| match env.event {
        maki_agent::AgentEvent::LiveToolBuf { id: got, body } if got == id => Some(body),
        _ => None,
    })
}

#[test]
fn start_annotation_timeout_happy_path() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "sa_to",
            description = "test",
            schema = {TIMEOUT_SCHEMA},
            start_annotation = {{ field = "timeout", kind = "timeout" }},
            handler = function(input, ctx) return "" end
        }})"#,
    );
    host.load_source("sa_to_plugin", &src).unwrap();
    let entry = reg.get("sa_to").expect("tool not registered");
    let inv = entry
        .tool
        .parse(&serde_json::json!({"timeout": 90}))
        .expect("parse failed");
    assert_eq!(inv.start_annotation(), Some(timeout_annotation(90)));
}

#[test]
fn start_annotation_count_happy_path() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "sa_ct",
            description = "test",
            schema = {ARRAY_SCHEMA},
            start_annotation = "edits",
            handler = function(input, ctx) return "" end
        }})"#,
    );
    host.load_source("sa_ct_plugin", &src).unwrap();
    let entry = reg.get("sa_ct").expect("tool not registered");
    let inv = entry
        .tool
        .parse(&serde_json::json!({"edits": [1, 2, 3]}))
        .expect("parse failed");
    assert_eq!(inv.start_annotation(), Some("3 edits".to_owned()));
}

#[test_case::test_case(START_ANNOTATION_COUNT_NON_ARRAY_SRC, STRING_NAME_SCHEMA, "not in schema properties or not type 'array'" ; "start_annotation_count_non_array")]
fn registration_with_schema_rejects(fields: &str, schema: &str, expected_err: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            {fields},
            schema = {schema},
            handler = function(input, ctx) return "" end
        }})"#,
    );
    let err = host
        .load_source("schema_val_test", &src)
        .expect_err("expected validation error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains(expected_err), "got: {err}");
}

#[test]
fn interpreter_on_output_streams_lines() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "interp_stream",
            description = "streams interpreter output",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local lines = {{}}
                local result, err = maki.interpreter.run("print('a')\nprint('b')", {{
                    timeout = 10,
                    max_memory_mb = 50,
                    on_output = function(line)
                        table.insert(lines, line)
                    end,
                }})
                if err then return "err: " .. err end
                return table.concat(lines, "|") .. ";stdout=" .. (result.stdout or "")
            end
        }})"#,
    );
    host.load_source("interp_stream_plugin", &src).unwrap();
    let out = exec_tool(&reg, "interp_stream", serde_json::json!({})).unwrap();
    assert_eq!(out, "a|b;stdout=a\nb");
}

const SESSION_CLOSED_ERR: &str = "session closed";

fn interp_tool_plugin(name: &str, python: &str, tools_lua: &str) -> String {
    format!(
        r#"maki.api.register_tool({{
            name = "{name}",
            description = "test",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local lines = {{}}
                local result, err = maki.interpreter.run("{python}", {{
                    timeout = 10,
                    max_memory_mb = 50,
                    on_output = function(line) table.insert(lines, line) end,
                    tools = {tools_lua},
                }})
                if err then return "err: " .. err end
                return table.concat(lines, "|")
            end
        }})"#
    )
}

#[test]
fn interpreter_tools_fn_map_kwargs_reach_lua_tool() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = interp_tool_plugin(
        "interp_tools",
        r"r = await greet(name='bob')\nprint(r)",
        "{ greet = function(input) return 'hi:' .. input.name end }",
    );
    host.load_source("interp_tools_plugin", &src).unwrap();
    let out = exec_tool(&reg, "interp_tools", serde_json::json!({})).unwrap();
    assert_eq!(out, "hi:bob");
}

#[test]
fn interpreter_tools_nil_err_pair_fails_call() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = interp_tool_plugin(
        "interp_err",
        r"await bad()",
        "{ bad = function(input) return nil, 'boom' end }",
    );
    host.load_source("interp_err_plugin", &src).unwrap();
    let out = exec_tool(&reg, "interp_err", serde_json::json!({})).unwrap();
    assert!(out.starts_with("err: "), "got: {out}");
    assert!(out.contains("boom"), "got: {out}");
}

#[test]
fn interpreter_tools_gather_resolves_parallel_batch() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = interp_tool_plugin(
        "interp_gather",
        r"import asyncio\nasync def main():\n    a, b = await asyncio.gather(t_a(), t_b())\n    print(a + '|' + b)\nawait main()",
        "{ t_a = function(input) return 'A' end, t_b = function(input) return 'B' end }",
    );
    host.load_source("interp_gather_plugin", &src).unwrap();
    let out = exec_tool(&reg, "interp_gather", serde_json::json!({})).unwrap();
    assert_eq!(out, "A|B");
}

const NESTED_DEPTH_TOOL: &str = "nested_depth";
const NESTED_DEPTH_BOTTOM: &str = "bottom";
const NESTED_DEPTH_WEDGED: &str =
    "nested call chain never replied: the in-flight gate charged a slot per level";
const NESTED_DEPTH_PLUGIN: &str = r#"
maki.api.register_tool({
    name = "nested_depth",
    description = "dispatches itself one level deeper",
    schema = {
        type = "object",
        properties = { depth = { type = "integer" } },
        required = { "depth" },
    },
    audiences = { "main" },
    handler = function(input, ctx)
        if input.depth == 0 then return "bottom" end
        local out, err = maki.agent.call_tool(ctx, "nested_depth", { depth = input.depth - 1 })
        if err then return { llm_output = err, is_error = true } end
        return out
    end,
})
"#;

/// Every level stays parked on its child, so a slot per level wedges the gate
/// for good once the chain is longer than the cap. A nested call rides its
/// caller's slot instead, which leaves the depth up to the callers.
#[test]
fn nested_calls_run_deeper_than_the_inflight_cap() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("nested_depth_plugin", NESTED_DEPTH_PLUGIN)
        .unwrap();

    let (done_tx, done_rx) = flume::bounded(1);
    let worker_reg = Arc::clone(&reg);
    std::thread::spawn(move || {
        let out = exec_tool_in(
            &worker_reg,
            NESTED_DEPTH_TOOL,
            json!({ "depth": MAX_INFLIGHT_TOOLS + 1 }),
            Some(Arc::clone(&worker_reg)),
        );
        let _ = done_tx.send(out);
    });

    let out = poll_until(NESTED_DEPTH_WEDGED, || done_rx.try_recv().ok());

    assert_eq!(out, Ok(NESTED_DEPTH_BOTTOM.to_owned()));
    drop(host);
}

#[test]
fn call_tool_resolves_lua_tool_and_reports_unknown() {
    let reg = Arc::clone(ToolRegistry::global_arc());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("echo_plugin", ECHO_PLUGIN).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "call_tool_probe",
            description = "test",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local out, err = maki.agent.call_tool(ctx, "echo_", {{ msg = "hello" }})
                if err ~= nil then return "unexpected err: " .. err end
                local out2, err2 = maki.agent.call_tool(ctx, "no_such_tool_xyz", {{}})
                if out2 ~= nil then return "unexpected output: " .. out2 end
                if err2 == nil then return "expected err for unknown tool" end
                return out
            end
        }})"#
    );
    host.load_source("call_tool_plugin", &src).unwrap();
    let out = exec_tool_in(
        &reg,
        "call_tool_probe",
        serde_json::json!({}),
        Some(Arc::clone(&reg)),
    )
    .unwrap();
    assert_eq!(out, "hello");
    host.unload("call_tool_plugin").unwrap();
    host.unload("echo_plugin").unwrap();
}

#[test]
fn session_close_idempotent_and_prompt_after_close_errors() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "session_probe",
            description = "test",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local sess = maki.agent.session(ctx, {{}})
                sess:close()
                sess:close()
                local result, err = sess:prompt("x")
                if result ~= nil then return "unexpected result" end
                return err or "no error"
            end
        }})"#
    );
    host.load_source("session_plugin", &src).unwrap();
    let out = exec_tool(&reg, "session_probe", serde_json::json!({})).unwrap();
    assert_eq!(out, SESSION_CLOSED_ERR);
}

#[test_case::test_case("{ audience = 'wurkflow' }", "unknown audience: wurkflow" ; "unknown_audience")]
#[test_case::test_case("{ local_tools = { foo = { handler = function() return '' end } } }", "local_tools.foo: 'description' is required" ; "local_tool_missing_description")]
#[test_case::test_case("{ local_tools = { foo = { description = 'd' } } }", "local_tools.foo: 'handler' is required" ; "local_tool_missing_handler")]
fn session_opts_validation_rejects(opts: &str, expected: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "session_opts_probe",
            description = "test",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local sess, err = maki.agent.session(ctx, {opts})
                if sess ~= nil then return "unexpected session" end
                return err or "no error"
            end
        }})"#
    );
    host.load_source("session_opts_plugin", &src).unwrap();
    let out = exec_tool(&reg, "session_opts_probe", serde_json::json!({})).unwrap();
    assert!(out.contains(expected), "got: {out}");
}

fn load_img_tool(host: &PluginHost) {
    let src = format!(
        r#"maki.api.register_tool({{
            name = "img_probe",
            description = "test",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                return {{
                    llm_output = "[image: test 1x1]",
                    image = {{ media_type = "image/png", data = "aGVsbG8=" }},
                }}
            end
        }})"#
    );
    host.load_source("img_plugin", &src).unwrap();
}

#[test]
fn lua_tool_image_reply_maps_to_image_output() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    load_img_tool(&host);
    let out = exec_tool_output(&reg, "img_probe", serde_json::json!({})).unwrap();
    let maki_agent::ToolOutput::Image { source, text } = out else {
        panic!("expected Image output, got {out:?}");
    };
    assert_eq!(source.media_type, maki_agent::ImageMediaType::Png);
    assert_eq!(&*source.data, "aGVsbG8=");
    assert_eq!(text, "[image: test 1x1]");
}

#[test]
fn call_tool_flattens_image_output_with_not_visible_note() {
    use maki_agent::tools::interpreter_bridge::IMAGE_NOT_VISIBLE_NOTE;

    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    load_img_tool(&host);
    let src = format!(
        r#"maki.api.register_tool({{
            name = "img_caller",
            description = "test",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local out, err = maki.agent.call_tool(ctx, "img_probe", {{}})
                return err or out
            end
        }})"#
    );
    host.load_source("img_caller_plugin", &src).unwrap();
    let out = exec_tool_in(
        &reg,
        "img_caller",
        serde_json::json!({}),
        Some(Arc::clone(&reg)),
    )
    .unwrap();
    assert_eq!(out, format!("[image: test 1x1] ({IMAGE_NOT_VISIBLE_NOTE})"));
}

#[test]
fn view_image_tool_returns_image_output() {
    use base64::Engine as _;

    let (reg, _host) = builtins_host();

    // The code_execution bridge flattens output to text, so view_image is
    // pointless from the interpreter.
    let audience = reg.get("view_image").unwrap().tool.audience();
    assert!(audience.contains(maki_agent::tools::ToolAudience::MAIN));
    assert!(!audience.contains(maki_agent::tools::ToolAudience::INTERPRETER));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tiny.png");
    let img = image::DynamicImage::new_rgb8(4, 2);
    img.save_with_format(&path, image::ImageFormat::Png)
        .unwrap();

    let out = exec_tool_output(
        &reg,
        "view_image",
        serde_json::json!({"path": path.to_str().unwrap()}),
    )
    .unwrap();
    let maki_agent::ToolOutput::Image { source, text } = out else {
        panic!("expected Image output, got {out:?}");
    };
    assert_eq!(source.media_type, maki_agent::ImageMediaType::Png);
    assert!(text.contains("tiny.png"), "caption: {text}");
    assert!(text.contains("4x2"), "caption: {text}");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&*source.data)
        .unwrap();
    assert_eq!(decoded, std::fs::read(&path).unwrap());
}

#[test]
fn view_image_tool_rejects_non_image() {
    let (reg, _host) = builtins_host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, "plain text").unwrap();
    let err = exec_tool_output(
        &reg,
        "view_image",
        serde_json::json!({"path": path.to_str().unwrap()}),
    )
    .unwrap_err();
    assert!(err.contains("not an image"), "got: {err}");
}

fn probe_output(data: &str) -> (image::ImageFormat, u32, u32) {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap();
    let reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .unwrap();
    let format = reader.format().unwrap();
    let (w, h) = reader.into_dimensions().unwrap();
    (format, w, h)
}

#[test]
fn view_image_downscales_oversized_png_with_honest_caption() {
    let (reg, _host) = builtins_host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wide.png");
    image::DynamicImage::new_rgb8(2000, 100)
        .save_with_format(&path, image::ImageFormat::Png)
        .unwrap();

    let out = exec_tool_output(
        &reg,
        "view_image",
        serde_json::json!({"path": path.to_str().unwrap()}),
    )
    .unwrap();
    let maki_agent::ToolOutput::Image { source, text } = out else {
        panic!("expected Image output, got {out:?}");
    };
    assert_eq!(source.media_type, maki_agent::ImageMediaType::Png);
    assert!(text.contains("downscaled from 2000x100"), "caption: {text}");

    let (format, w, h) = probe_output(&source.data);
    assert_eq!(format, image::ImageFormat::Png);
    assert_eq!(w, 1568, "long edge must land exactly on the API limit");
    assert!(h <= 79, "aspect ratio broken: {w}x{h}");
    // Caption must report the dimensions actually shipped, not the original.
    assert!(text.contains(&format!("{w}x{h}")), "caption: {text}");
}

#[test]
fn view_image_oversized_gif_reencodes_to_png_first_frame() {
    let (reg, _host) = builtins_host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("banner.gif");
    image::DynamicImage::new_rgb8(2000, 8)
        .save_with_format(&path, image::ImageFormat::Gif)
        .unwrap();

    let out = exec_tool_output(
        &reg,
        "view_image",
        serde_json::json!({"path": path.to_str().unwrap()}),
    )
    .unwrap();
    let maki_agent::ToolOutput::Image { source, text } = out else {
        panic!("expected Image output, got {out:?}");
    };
    // gif encoding is unsupported, so downscaling forces png; the caption
    // must confess the downscale and the lost animation.
    assert_eq!(source.media_type, maki_agent::ImageMediaType::Png);
    assert!(text.contains("downscaled from 2000x8"), "caption: {text}");
    assert!(text.contains("first frame only"), "caption: {text}");
    assert_eq!(probe_output(&source.data).0, image::ImageFormat::Png);
}

#[test]
fn view_image_small_gif_passes_through_unchanged() {
    use base64::Engine as _;

    let (reg, _host) = builtins_host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tiny.gif");
    image::DynamicImage::new_rgb8(4, 2)
        .save_with_format(&path, image::ImageFormat::Gif)
        .unwrap();

    let out = exec_tool_output(
        &reg,
        "view_image",
        serde_json::json!({"path": path.to_str().unwrap()}),
    )
    .unwrap();
    let maki_agent::ToolOutput::Image { source, text } = out else {
        panic!("expected Image output, got {out:?}");
    };
    assert_eq!(source.media_type, maki_agent::ImageMediaType::Gif);
    assert!(
        !text.contains("first frame only"),
        "pass-through keeps animation, caption must not claim otherwise: {text}"
    );
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&*source.data)
        .unwrap();
    assert_eq!(
        decoded,
        std::fs::read(&path).unwrap(),
        "under-limit gif must ship byte-identical, not re-encoded"
    );
}

#[test]
fn interpreter_bridge_flattens_image_with_visibility_note() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    load_img_tool(&host);

    let mut ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    ctx.registry = Arc::clone(&reg);
    let out = smol::block_on(maki_agent::tools::interpreter_bridge::dispatch(
        &ctx,
        "img_probe",
        &serde_json::json!({}),
    ))
    .unwrap();
    assert!(out.starts_with("[image: test 1x1]"), "got: {out}");
    assert!(
        out.contains(maki_agent::tools::interpreter_bridge::IMAGE_NOT_VISIBLE_NOTE),
        "got: {out}"
    );
}

/// The sessions picker parks its command handler in a `win:recv` loop while a
/// `maki.async.run` task fetches the stored-session list. Queued async tasks
/// must run while the spawning handler is still parked, not wait for the next
/// unrelated lua-thread event.
#[test]
fn async_run_from_parked_command_handler_runs_promptly() {
    let host = PluginHost::new(fresh_registry()).unwrap();
    host.load_source(
        "p",
        r#"
        maki.api.register_command({
            name = "/park",
            description = "parks forever",
            handler = function()
                maki.async.run(function()
                    maki.ui.flash("task-ran")
                end)
                maki.async.await(1, function(_cb) end)
            end,
        })
        "#,
    )
    .unwrap();
    let rx = host.ui_action_rx();
    let handle = host.event_handle();
    handle.run_command(Arc::from("p"), Arc::from("/park"), String::new(), 0);

    let action = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("async.run task starved while its command handler was parked");
    assert!(matches!(action, maki_lua::UiAction::Flash(msg) if msg == "task-ran"));
}

/// Job callbacks must fire while a detached command handler is parked
/// (the homepage `/standup` example: jobstart, then a `win:recv` loop).
#[test]
fn job_callbacks_fire_while_command_handler_parked() {
    let host = PluginHost::new(fresh_registry()).unwrap();
    host.load_source(
        "p",
        r#"
        maki.api.register_command({
            name = "/stream",
            description = "streams job output while parked",
            handler = function()
                maki.fn.jobstart("echo hi", {
                    on_stdout = function(_, line) maki.ui.flash("job:" .. line) end,
                })
                maki.async.await(1, function(_cb) end)
            end,
        })
        "#,
    )
    .unwrap();
    let rx = host.ui_action_rx();
    let handle = host.event_handle();
    handle.run_command(Arc::from("p"), Arc::from("/stream"), String::new(), 0);

    let action = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("job callbacks starved while command handler was parked");
    assert!(matches!(action, maki_lua::UiAction::Flash(msg) if msg == "job:hi"));
}

/// read tool requires offset and limit; missing fields should fail schema validation.
mod read_tool_required_params {
    use super::*;

    const MISSING_OFFSET_ERR: &str = "invalid parameter 'offset': required";
    const MISSING_LIMIT_ERR: &str = "invalid parameter 'limit': required";
    const MAX_OUTPUT_LINES: i32 = 2000;

    #[test]
    fn missing_offset_fails_parse() {
        let (reg, _host) = builtins_host();
        let entry = reg.get("read").expect("read registered");
        let err = entry
            .tool
            .parse(&serde_json::json!({ "path": "/tmp/foo.txt", "limit": 10 }))
            .err()
            .expect("missing offset should fail");
        assert!(err.to_string().contains(MISSING_OFFSET_ERR), "got: {err}");
    }

    #[test]
    fn missing_limit_fails_parse() {
        let (reg, _host) = builtins_host();
        let entry = reg.get("read").expect("read registered");
        let err = entry
            .tool
            .parse(&serde_json::json!({ "path": "/tmp/foo.txt", "offset": 1 }))
            .err()
            .expect("missing limit should fail");
        assert!(err.to_string().contains(MISSING_LIMIT_ERR), "got: {err}");
    }

    #[test]
    fn both_offset_and_limit_present_parses() {
        let (reg, _host) = builtins_host();
        let entry = reg.get("read").expect("read registered");
        let result = entry.tool.parse(&serde_json::json!({
            "path": "/tmp/foo.txt",
            "offset": 1,
            "limit": 10
        }));
        assert!(result.is_ok(), "valid input should parse");
    }

    #[test]
    fn limit_zero_reads_to_end_with_right_aligned_line_numbers() {
        let (reg, _host) = builtins_host();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        let content = (1..=100i32)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &content).unwrap();

        let out = exec_tool(
            &reg,
            "read",
            serde_json::json!({
                "path": path.to_str().unwrap(),
                "offset": 1,
                "limit": 0
            }),
        );
        let out = out.expect("read should succeed");
        assert!(
            out.starts_with("  1: line 1\n"),
            "line 1 must be padded to the width of line 100, got: {out}"
        );
        assert!(
            out.ends_with("\n100: line 100"),
            "limit=0 should read to the end, got: {out}"
        );
    }

    #[test]
    fn limit_zero_respects_2000_cap() {
        let (reg, _host) = builtins_host();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.txt");
        // Write 2500 lines
        let content = (1..=2500i32)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &content).unwrap();

        let out = exec_tool(
            &reg,
            "read",
            serde_json::json!({
                "path": path.to_str().unwrap(),
                "offset": 1,
                "limit": 0
            }),
        );
        let out = out.expect("read should succeed");
        // limit=0 capped at 2000, so should have lines 1-2000
        assert!(out.contains("1: line 1"), "should start at line 1");
        assert!(
            out.contains("2000: line 2000"),
            "should include line 2000, got last lines: {}",
            out.split('\n').rev().take(5).collect::<Vec<_>>().join("\n")
        );
        assert!(
            !out.contains("2001: line 2001"),
            "should not include line 2001"
        );
        // Should have truncation hint
        assert!(
            out.contains("Truncated"),
            "should mention truncation, got: {out}"
        );
    }

    #[test]
    fn explicit_limit_above_cap_is_clamped() {
        let (reg, _host) = builtins_host();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.txt");
        let content = (1..=MAX_OUTPUT_LINES + 500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &content).unwrap();

        let out = exec_tool(
            &reg,
            "read",
            serde_json::json!({
                "path": path.to_str().unwrap(),
                "offset": 1,
                "limit": MAX_OUTPUT_LINES + 500
            }),
        );
        let out = out.expect("read should succeed");
        assert!(
            out.contains(&format!("{MAX_OUTPUT_LINES}: line {MAX_OUTPUT_LINES}")),
            "should include the last line within the cap, got: {out}"
        );
        assert!(
            !out.contains(&format!(
                "{}: line {}",
                MAX_OUTPUT_LINES + 1,
                MAX_OUTPUT_LINES + 1
            )),
            "explicit limit must be clamped to the cap, got: {out}"
        );
    }

    #[test]
    fn offset_beyond_file_returns_empty() {
        let (reg, _host) = builtins_host();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, "just one line\n").unwrap();

        let out = exec_tool(
            &reg,
            "read",
            serde_json::json!({
                "path": path.to_str().unwrap(),
                "offset": 100,
                "limit": 10
            }),
        );
        let out = out.expect("read should succeed");
        assert!(
            out.is_empty(),
            "offset beyond file should return empty, got: {out}"
        );
    }
}

#[test]
fn jobwait_reentrant_self_wait_in_on_exit() {
    let (reg, host) = builtins_host();
    let session = maki_storage::id::MakiId::generate();
    let src = format!(
        r#"
local job_id
maki.api.register_tool({{
    name = "start_self_wait_job",
    description = "session job whose on_exit reenters jobwait on itself",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        job_id = maki.fn.jobstart("sleep 1", {{
            scope = {{ session = "{session}" }},
            on_exit = function(id, code)
                local ok, res = pcall(maki.fn.jobwait, id, 2000)
                if not ok then
                    error("self-wait errored: " .. tostring(res))
                end
                if res == nil then
                    error("self-wait timed out")
                end
                if res.exit_code ~= code then
                    error("self-wait code mismatch")
                end
            end,
        }})
        return tostring(job_id)
    end,
}})
maki.api.register_tool({{
    name = "wait_self_wait_job",
    description = "outer wait like monitor_wait",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        local ok, res = pcall(maki.fn.jobwait, job_id, 10000)
        if not ok then
            return {{ llm_output = "error: " .. tostring(res), is_error = true }}
        end
        if res == nil then
            return {{ llm_output = "error: outer wait timed out", is_error = true }}
        end
        return "exit:" .. tostring(res.exit_code)
    end,
}})
"#
    );
    host.load_source("selfwait", &src).unwrap();

    exec_tool(&reg, "start_self_wait_job", json!({})).unwrap();
    let out = exec_tool(&reg, "wait_self_wait_job", json!({})).unwrap();
    assert_eq!(
        out, "exit:0",
        "reentrant self-wait must not poison the outer wait"
    );
    let after = exec_tool(&reg, "wait_self_wait_job", json!({})).unwrap();
    assert!(
        after.starts_with("exit:"),
        "VM must stay usable, got: {after}"
    );
}

#[test]
fn jobwait_returns_after_session_end_kill() {
    let (reg, host) = builtins_host();
    let session = maki_storage::id::MakiId::generate();
    let dir = tempfile::tempdir().unwrap();
    let parked_path = dir.path().join("parked");
    let src = format!(
        r#"
maki.api.register_tool({{
    name = "wait_long_job",
    description = "parks in jobwait until the session ends",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        -- jobwait checks the job's output channel out of the store, so only
        -- a parked jobwait can run this callback. The marker is proof the
        -- wait is really parked, where a sleep would just be a guess.
        local id = maki.fn.jobstart("echo parked; exec sleep 30", {{
            scope = {{ session = "{session}" }},
            on_stdout = function() maki.fs.write("{parked}", "parked") end,
        }})
        local ok, res = pcall(maki.fn.jobwait, id, 25000)
        if not ok then
            return {{ llm_output = "error: " .. tostring(res), is_error = true }}
        end
        if res == nil then
            return {{ llm_output = "error: jobwait timed out", is_error = true }}
        end
        return "exit:" .. tostring(res.exit_code)
    end,
}})
"#,
        parked = parked_path.display(),
    );
    host.load_source("endkill", &src).unwrap();

    let reg2 = Arc::clone(&reg);
    let wait_handle =
        std::thread::spawn(move || exec_tool(&reg2, "wait_long_job", json!({})).unwrap());
    poll_until("jobwait never parked", || {
        parked_path.exists().then_some(())
    });
    host.event_handle()
        .end_sessions_blocking([session], SessionEndReason::Shutdown);
    let out = wait_handle.join().expect("wait thread must not panic");
    assert!(
        out.starts_with("exit:"),
        "parked jobwait must collect the exit of the killed job, got: {out}"
    );
}

#[test]
fn jobwait_callback_error_still_delivers_exit() {
    let (reg, host) = builtins_host();
    let session = maki_storage::id::MakiId::generate();
    let src = format!(
        r#"
local job_id
maki.api.register_tool({{
    name = "start_boom_job",
    description = "job whose on_stdout raises",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        job_id = maki.fn.jobstart("echo one; echo two; echo three", {{
            scope = {{ session = "{session}" }},
            on_stdout = function(_, line)
                if line == "two" then
                    error("boom on stdout")
                end
            end,
        }})
        return tostring(job_id)
    end,
}})
maki.api.register_tool({{
    name = "wait_boom_job",
    description = "waits past the failing callback",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        local ok, res = pcall(maki.fn.jobwait, job_id, 10000)
        if not ok then
            return {{ llm_output = "error: " .. tostring(res), is_error = true }}
        end
        if res == nil then
            return {{ llm_output = "error: timed out", is_error = true }}
        end
                return "exit:" .. tostring(res.exit_code) .. "|stdout:" .. tostring(res.stdout)
    end,
}})
"#
    );
    host.load_source("boomjob", &src).unwrap();
    exec_tool(&reg, "start_boom_job", json!({})).unwrap();
    let out = exec_tool(&reg, "wait_boom_job", json!({})).unwrap();
    assert!(
        out.starts_with("exit:0"),
        "a failing on_stdout must not swallow the exit, got: {out}"
    );
    let again = exec_tool(&reg, "wait_boom_job", json!({})).unwrap();
    assert!(
        again.starts_with("exit:"),
        "VM must stay usable, got: {again}"
    );
}
