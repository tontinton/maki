#[cfg(unix)]
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Command;
use std::sync::Arc;

use futures_lite::io::{AsyncReadExt, AsyncWriteExt};

use maki_agent::agent::tool_dispatch;
use maki_agent::permissions::PermissionManager;
use maki_agent::tools::interpreter_bridge;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{CallOrigin, ToolContext, ToolRegistry};
use maki_agent::{AgentEvent, AgentMode, EventSender};
use maki_config::{
    DEFAULT_BUILTINS, Effect, PermissionRule, PermissionsConfig, PluginFileConfig, PluginsConfig,
    ProjectConfig, ToolKey,
};
use maki_lua::{PluginHost, set_allowed_private_hosts};
use serde_json::{Value, json};
use smol::net::TcpListener;

const TOOLS: &[&str] = &[
    "bash",
    "code_execution",
    "read",
    "write",
    "grep",
    "glob",
    "batch",
    "webfetch",
];
const MODEL_BYTES: usize = 1024;
const MODEL_LINES: usize = 8;
const COMPACT_GLOB_LIMIT: usize = 100;
const TEST_TIMEOUT_SECS: u64 = 60;
const LAST_ITEM: &str = "final-item";
const CALL_ID: &str = "output-test";
const PROMPT_PLUGIN: &str = "guard";
const ALLOW_EVERY_PROMPT: &str =
    r#"maki.api.set_slot("permission.prompt", function() return { decision = "allow" } end)"#;
#[cfg(unix)]
const RTK_FIXTURE_DIR: &str = "MAKI_TEST_RTK_DIR";
#[cfg(unix)]
const RTK_LOG: &str = "MAKI_TEST_RTK_LOG";
#[cfg(unix)]
const RTK_COMMAND: &str = "printf original";
const RELAY: &str = r#"
maki.api.register_tool({
    name = "relay_read",
    description = "nested read fixture",
    schema = {
        type = "object",
        properties = { path = { type = "string" }, mode = { type = "string" } },
    },
    handler = function(input, ctx)
        local out, err = maki.agent.call_tool(ctx, "read", {
            path = input.path, offset = 1, limit = 0,
        }, input.mode and { output_mode = input.mode } or nil)
        return { llm_output = out or err, is_error = err ~= nil }
    end,
})
maki.api.register_tool({
    name = "relay_batch",
    description = "nested batch fixture",
    schema = { type = "object", properties = { path = { type = "string" } } },
    handler = function(input, ctx)
        return maki.agent.call_tool(ctx, "batch", { tool_calls = {
            { tool = "read", parameters = { path = input.path, offset = 1, limit = 0 } },
        } })
    end,
})
"#;

fn setup() -> (Arc<ToolRegistry>, PluginHost, ToolContext) {
    let registry = Arc::new(ToolRegistry::new());
    let mut host = PluginHost::new(Arc::clone(&registry)).unwrap();
    let plugins = DEFAULT_BUILTINS
        .iter()
        .filter(|name| !TOOLS.contains(name))
        .map(|name| {
            (
                (*name).to_owned(),
                PluginFileConfig {
                    enabled: Some(false),
                    ..Default::default()
                },
            )
        })
        .collect();
    host.load_builtins(&PluginsConfig::from_plugins(plugins))
        .unwrap();
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.registry = Arc::clone(&registry);
    ctx.config.rtk = false;
    ctx.config.max_output_bytes = MODEL_BYTES;
    ctx.config.max_output_lines = MODEL_LINES;
    (registry, host, ctx)
}

fn run(ctx: &ToolContext, name: &str, input: Value) -> Result<String, String> {
    let done = smol::block_on(tool_dispatch::run(
        CALL_ID.into(),
        name,
        &input,
        ctx,
        CallOrigin::Model,
    ));
    interpreter_bridge::flatten(&done)
}

fn python(ctx: &ToolContext, code: &str) -> Result<String, String> {
    run(
        ctx,
        "code_execution",
        json!({ "code": code, "timeout": TEST_TIMEOUT_SECS }),
    )
}

#[test]
#[cfg(unix)]
fn python_parses_complete_bash_json_with_bounded_final_output() {
    let (_registry, _host, ctx) = setup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("payload.json");
    let mut items = vec!["x".repeat(MODEL_BYTES); 128];
    items.push(LAST_ITEM.to_owned());
    let payload = serde_json::to_string(&items).unwrap();
    fs::write(&path, &payload).unwrap();
    let command = format!("cat '{}'", path.display());
    let code = format!(
        "text = await bash(command={command:?})\ndata = json.loads(text)\nprint(len(text), len(data), data[-1])"
    );
    let output = python(&ctx, &code).expect("Python must parse the complete command result");
    assert_eq!(
        output.trim(),
        format!("{} {} {LAST_ITEM}", payload.len(), items.len())
    );
    assert!(output.len() < MODEL_BYTES);
    let model_output = run(&ctx, "bash", json!({ "command": command })).unwrap();
    assert!(model_output.contains("[truncated"));
    assert!(!model_output.contains(LAST_ITEM));
    let returned = python(&ctx, &format!("await bash(command={command:?})")).unwrap();
    assert!(returned.starts_with("return: "));
    assert!(returned.contains("[truncated"));
    assert!(returned.len() < MODEL_BYTES * 2);
}

#[test]
fn final_python_output_keeps_model_limits() {
    let (_registry, _host, ctx) = setup();
    let output = python(&ctx, &format!("print('x' * {})", MODEL_BYTES * 2)).unwrap();
    assert!(output.contains("[truncated"));
    assert!(output.len() < MODEL_BYTES * 2);
    let output = python(
        &ctx,
        &format!(
            "print('\\n'.join(str(i) for i in range({})))",
            MODEL_LINES * 3
        ),
    )
    .unwrap();
    assert!(output.contains("[truncated"));
    assert!(!output.contains(&format!("\n{}\n", MODEL_LINES * 3 - 1)));
}

#[test_case::test_case(1, 0, MODEL_LINES * 3 ; "through_eof")]
#[test_case::test_case(3, MODEL_LINES + 2, MODEL_LINES + 2 ; "explicit_range")]
fn python_read_preserves_long_lines_and_requested_range(offset: usize, limit: usize, count: usize) {
    let (_registry, _host, ctx) = setup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long-lines.txt");
    let lines: Vec<String> = (0..MODEL_LINES * 3)
        .map(|i| format!("{}record-{i}", "x".repeat(MODEL_BYTES)))
        .collect();
    fs::write(&path, lines.join("\n")).unwrap();
    let expected_last = &lines[offset + count - 2];
    let code = format!(
        "text = await read(path={:?}, offset={offset}, limit={limit})\nlines = [line.split(': ', 1)[1] for line in text.splitlines() if re.match(r'^\\s*\\d+: ', line)]\nprint(len(lines), lines[-1] == {expected_last:?})",
        path.to_str().unwrap()
    );
    assert_eq!(python(&ctx, &code).unwrap().trim(), format!("{count} True"));
    let model_output = run(
        &ctx,
        "read",
        json!({ "path": path, "offset": offset, "limit": limit }),
    )
    .unwrap();
    assert!(!model_output.contains(expected_last));
}

#[test]
fn python_grep_preserves_match_and_context_text() {
    let (_registry, _host, ctx) = setup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grep.txt");
    let context = "c".repeat(MODEL_BYTES) + "context-end";
    let matched = "m".repeat(MODEL_BYTES) + "needle-end";
    fs::write(&path, format!("{context}\n{matched}\n{context}")).unwrap();
    let code = format!(
        "text = await grep(pattern='needle', path={:?}, context_before=1, context_after=1, limit=1)\nprint(text.count({context:?}), {matched:?} in text)",
        path.to_str().unwrap()
    );
    assert_eq!(python(&ctx, &code).unwrap().trim(), "2 True");
    let model_output = run(
        &ctx,
        "grep",
        json!({ "pattern": "needle", "path": path,
        "context_before": 1, "context_after": 1, "limit": 1 }),
    )
    .unwrap();
    assert!(!model_output.contains("needle-end"));
}

#[test]
fn python_glob_preserves_all_results_beyond_compact_limit() {
    let (_registry, _host, mut ctx) = setup();
    let dir = tempfile::tempdir().unwrap();
    let count = COMPACT_GLOB_LIMIT + MODEL_LINES * 3;
    for i in 0..count {
        fs::write(dir.path().join(format!("record-{i}.txt")), "").unwrap();
    }
    let code = format!(
        "paths = (await glob(pattern='*.txt', path={:?})).splitlines()\nprint(len(paths))",
        dir.path().to_str().unwrap()
    );
    assert_eq!(python(&ctx, &code).unwrap().trim(), count.to_string());
    assert!(
        run(
            &ctx,
            "glob",
            json!({ "pattern": "*.txt", "path": dir.path() })
        )
        .unwrap()
        .contains("[truncated")
    );
    ctx.config.max_output_lines = count;
    ctx.config.max_output_bytes = count * MODEL_BYTES;
    let compact = run(
        &ctx,
        "glob",
        json!({ "pattern": "*.txt", "path": dir.path() }),
    )
    .unwrap();
    assert_eq!(compact.lines().count(), COMPACT_GLOB_LIMIT);
}

#[test]
#[cfg(unix)]
fn python_bash_returns_full_output_and_keeps_complete_error_output() {
    let (_registry, _host, ctx) = setup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lines.txt");
    let lines: Vec<String> = (0..MODEL_LINES * 3)
        .map(|i| format!("record-{i}"))
        .collect();
    fs::write(&path, lines.join("\n")).unwrap();
    let command = format!("cat '{}'", path.display());
    let code = format!("text = await bash(command={command:?})\nprint(len(text.splitlines()))");
    assert_eq!(python(&ctx, &code).unwrap().trim(), lines.len().to_string());
    let failing = command + "; exit 7";
    let code = format!(
        "try:\n    await bash(command={failing:?})\nexcept RuntimeError as error:\n    print({:?} in str(error), 'Exit code: 7' in str(error))",
        lines.last().unwrap()
    );
    assert_eq!(python(&ctx, &code).unwrap().trim(), "True True");
    let uncaught = python(&ctx, &format!("await bash(command={failing:?})")).unwrap_err();
    assert!(uncaught.contains("[truncated"));
    assert!(uncaught.len() < MODEL_BYTES);
    assert!(!uncaught.contains(lines.last().unwrap()));
}

#[test]
fn lua_calls_default_to_full_with_isolated_compact_overrides() {
    let (_registry, host, ctx) = setup();
    host.load_source("relay", RELAY).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.txt");
    let count = MODEL_LINES * 3;
    let content = (0..count)
        .map(|i| format!("record-{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, &content).unwrap();
    let full = run(&ctx, "relay_read", json!({ "path": path })).unwrap();
    assert_eq!(
        full.lines()
            .filter(|line| line.contains(": record-"))
            .count(),
        count
    );
    let code = format!(
        "a, b = await gather(relay_read(path={0:?}), relay_read(path={0:?}, mode='compact'))\nprint(len([line for line in a.splitlines() if ': record-' in line]), len([line for line in b.splitlines() if ': record-' in line]))",
        path.to_str().unwrap()
    );
    assert_eq!(
        python(&ctx, &code).unwrap().trim(),
        format!("{count} {MODEL_LINES}")
    );
    let batch = run(
        &ctx,
        "batch",
        json!({ "tool_calls": [{ "tool": "read", "parameters": {
            "path": path, "offset": 1, "limit": 0
        }}] }),
    )
    .unwrap();
    assert!(batch.contains("Truncated lines"));
    assert!(!batch.contains(&format!("record-{}", count - 1)));
    let full_batch = run(&ctx, "relay_batch", json!({ "path": path })).unwrap();
    assert_eq!(
        full_batch
            .lines()
            .filter(|line| line.contains(": record-"))
            .count(),
        count
    );
    let error = python(
        &ctx,
        &format!(
            "await relay_read(path={:?}, mode='invalid')",
            path.to_str().unwrap()
        ),
    )
    .unwrap_err();
    assert!(error.contains("output_mode must be 'full' or 'compact'"));
}

#[test]
fn programmatic_output_passes_through_output_hooks_once() {
    let (_registry, host, ctx) = setup();
    host.load_source(
        "redact",
        &format!(
            r#"
local calls = 0
maki.api.set_slot("tool.read.output", function(_, output, ctx)
    calls = calls + 1
    output.text = ctx.output_mode == "full" and output.text:find("{LAST_ITEM}", 1, true)
        and "redacted:" .. calls or "incomplete"
    return output
end)
"#
        ),
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private.txt");
    fs::write(&path, "private\n".repeat(MODEL_LINES * 3) + LAST_ITEM).unwrap();
    let code = format!(
        "print(await read(path={:?}, offset=1, limit=0))",
        path.to_str().unwrap()
    );
    assert_eq!(python(&ctx, &code).unwrap().trim(), "redacted:1");
}

#[test]
fn programmatic_input_hooks_can_deny_the_call() {
    let (_registry, host, ctx) = setup();
    host.load_source(
        "deny",
        r#"
maki.api.set_slot("tool.read.input", function() return nil, "read blocked" end)
"#,
    )
    .unwrap();
    let error = python(&ctx, "await read(path='does-not-exist', offset=1, limit=0)").unwrap_err();
    assert!(error.contains("read blocked"));
    assert!(!error.contains("read error"));
}

#[test]
fn programmatic_mode_keeps_permission_denials() {
    let (_registry, _host, mut ctx) = setup();
    let dir = tempfile::tempdir().unwrap();
    ctx.permissions = Arc::new(PermissionManager::new(
        PermissionsConfig {
            rules: vec![PermissionRule {
                tool: ToolKey::native("write"),
                scope: None,
                effect: Effect::Deny,
            }],
            ..Default::default()
        },
        dir.path().to_owned(),
        ProjectConfig::discover(dir.path()),
        Arc::default(),
    ));
    let path = dir.path().join("secret.txt");
    fs::write(&path, LAST_ITEM).unwrap();
    let error = python(
        &ctx,
        &format!(
            "await write(path={:?}, content='changed')",
            path.to_str().unwrap()
        ),
    )
    .unwrap_err();
    assert!(!error.contains(LAST_ITEM));
    assert!(error.contains("denied"), "{error}");
    assert_eq!(fs::read_to_string(&path).unwrap(), LAST_ITEM);
}

/// Nobody saw a prompt for the write inside the script, so the code_execution
/// row is where the user learns a plugin let it through.
#[test]
fn a_write_a_plugin_allowed_inside_python_marks_the_code_execution_row() {
    let (_registry, host, mut ctx) = setup();
    host.load_source(PROMPT_PLUGIN, ALLOW_EVERY_PROMPT).unwrap();
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    // A write outside the project prompts, and nobody is here to answer it.
    ctx.permissions = Arc::new(PermissionManager::new(
        PermissionsConfig::default(),
        project.path().to_owned(),
        ProjectConfig::discover(project.path()),
        Arc::default(),
    ));
    let (event_tx, events) = flume::unbounded();
    ctx.event_tx = EventSender::new(event_tx, 0);
    ctx.tool_use_id = Some(CALL_ID.to_owned());
    let path = outside.path().join("allowed.txt");

    python(
        &ctx,
        &format!(
            "await write(path={:?}, content={LAST_ITEM:?})",
            path.to_str().unwrap()
        ),
    )
    .expect("the plugin allowed the write");

    assert_eq!(fs::read_to_string(&path).unwrap(), LAST_ITEM);
    let marked: Vec<(String, String)> = events
        .try_iter()
        .filter_map(|envelope| match envelope.event {
            AgentEvent::AllowedByPlugin { id, plugin } => Some((id, plugin)),
            _ => None,
        })
        .collect();
    assert_eq!(marked, [(CALL_ID.to_owned(), PROMPT_PLUGIN.to_owned())]);
}

#[test]
fn python_webfetch_parses_complete_response_text() {
    let (_registry, _host, ctx) = setup();
    set_allowed_private_hosts(&["127.0.0.1".to_owned()]);
    let listener = smol::block_on(TcpListener::bind(("127.0.0.1", 0))).unwrap();
    let url = format!("http://{}/payload", listener.local_addr().unwrap());
    let payload = json!({ "padding": "x".repeat(MODEL_BYTES * 2), "last": LAST_ITEM }).to_string();
    let server = smol::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let code = format!(
        "data = json.loads(await webfetch(url={url:?}, format='text'))\nprint(data['last'])"
    );
    let output = python(&ctx, &code).unwrap();
    smol::block_on(server);
    assert_eq!(output.trim(), LAST_ITEM);
}

#[test]
#[cfg(unix)]
fn programmatic_bash_skips_automatic_rtk_rewriting() {
    if let Some(dir) = env::var_os(RTK_FIXTURE_DIR) {
        let (_registry, _host, mut ctx) = setup();
        ctx.config.rtk = true;
        assert_eq!(
            run(&ctx, "bash", json!({ "command": RTK_COMMAND })).unwrap(),
            "rewritten"
        );
        let log_path = Path::new(&dir).join("calls.txt");
        let before = fs::read_to_string(&log_path).unwrap();
        assert!(before.contains("--version"));
        assert!(before.contains("rewrite"));
        assert_eq!(
            python(&ctx, &format!("print(await bash(command={RTK_COMMAND:?}))"))
                .unwrap()
                .trim(),
            "original"
        );
        assert_eq!(fs::read_to_string(&log_path).unwrap(), before);
        assert_eq!(
            python(&ctx, "print(await bash(command='rtk emit'))")
                .unwrap()
                .trim(),
            "explicit"
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let rtk = dir.path().join("rtk");
    fs::write(
        &rtk,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$MAKI_TEST_RTK_LOG"
case "$1" in
  --version) printf 'rtk fixture\n' ;;
  rewrite) printf 'printf rewritten\n' ;;
  emit) printf 'explicit\n' ;;
  *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&rtk, fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![dir.path().to_owned()];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    let output = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "programmatic_bash_skips_automatic_rtk_rewriting",
            "--nocapture",
        ])
        .env(RTK_FIXTURE_DIR, dir.path())
        .env(RTK_LOG, dir.path().join("calls.txt"))
        .env("PATH", env::join_paths(paths).unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
