#![cfg(unix)]

use std::sync::Arc;

use maki_agent::AgentMode;
use maki_agent::agent::tool_dispatch;
use maki_agent::tools::interpreter_bridge;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{CallOrigin, ToolContext, ToolOutputMode, ToolRegistry};
use maki_config::{DEFAULT_BUILTINS, PluginFileConfig, PluginsConfig};
use maki_lua::PluginHost;
use maki_lua::test_support::runtime;
use serde_json::{Value, json};
use test_case::test_case;

const MODEL_BYTES: usize = 1024;
const MODEL_LINES: usize = 8;
const HEAD_BYTES: usize = MODEL_BYTES.div_ceil(2);
const STREAM_BYTES: usize = 600 * 1024 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
const CHUNK_COUNT: usize = STREAM_BYTES / CHUNK_BYTES;
/// `MAX_READ_BYTES`, the default job line cap and the full-mode byte ceiling.
const FULL_OUTPUT_MAX_BYTES: usize = 64 * 1024 * 1024;
const RECORD_COUNT: usize = MODEL_LINES * 3;
const TIMEOUT_SECS: usize = 120;
const CALL_ID: &str = "output-capture-test";
const FULL_LIMIT_NOTE: &str =
    "[output passed the full-mode limit; redirect it to a file to keep all of it]";
const FINAL_MARKER: &str = "final-output-marker";
const RECOVERY_MARKER: &str = "subsequent-command-completed";
const TRUNCATION_PREFIX: &str = "[truncated ";
const FAILURE_CODE: usize = 7;
const FAILURE_STATUS: &str = "Exit code: 7";

fn setup(output_mode: ToolOutputMode) -> (PluginHost, ToolContext) {
    let registry = Arc::new(ToolRegistry::new());
    let mut host = PluginHost::new(Arc::clone(&registry)).unwrap();
    let plugins = DEFAULT_BUILTINS
        .iter()
        .filter(|name| **name != "bash")
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
    ctx.registry = registry;
    ctx.output_mode = output_mode;
    ctx.config.rtk = false;
    ctx.config.max_output_bytes = MODEL_BYTES;
    ctx.config.max_output_lines = MODEL_LINES;
    (host, ctx)
}

fn run(ctx: &ToolContext, input: Value) -> Result<String, String> {
    let done = runtime().block_on(tool_dispatch::run(
        CALL_ID.into(),
        "bash",
        &input,
        ctx,
        CallOrigin::Model,
    ));
    interpreter_bridge::flatten(&done)
}

fn run_long(ctx: &ToolContext, command: String) -> String {
    run(ctx, json!({ "command": command, "timeout": TIMEOUT_SECS }))
        .expect("large output must finish without exhausting the Lua heap")
}

fn assert_host_recovers(ctx: &ToolContext) {
    assert_eq!(
        run(
            ctx,
            json!({ "command": format!("printf '{RECOVERY_MARKER}'") })
        )
        .expect("the same Lua host must remain usable after large output"),
        RECOVERY_MARKER
    );
}

/// Megabytes of one repeated byte would bury a failure message, so the run
/// at the start is shown as a count.
fn squeeze(text: &str) -> String {
    let Some(first) = text.bytes().next() else {
        return String::new();
    };
    let run = text.bytes().take_while(|byte| *byte == first).count();
    format!("{run} x {:?} then {:?}", first as char, &text[run..])
}

fn marker(lines: usize, bytes: usize) -> String {
    format!("{TRUNCATION_PREFIX}{lines} lines, {bytes} bytes]")
}

fn large_output_command(single_line: bool) -> String {
    let newline = if single_line { "" } else { "\\n" };
    let chunk_bytes = CHUNK_BYTES - usize::from(!single_line);
    format!(
        "awk 'BEGIN {{ chunk = sprintf(\"%0{chunk_bytes}d\", 0); for (i = 0; i < {CHUNK_COUNT}; i++) printf \"%s{newline}\", chunk; printf \"%s\", \"{FINAL_MARKER}\" }}'"
    )
}

fn records() -> Vec<String> {
    (0..RECORD_COUNT).map(|i| format!("record-{i}")).collect()
}

fn both_ends(records: &[String]) -> String {
    let head_lines = MODEL_LINES.div_ceil(2);
    let tail_lines = MODEL_LINES - head_lines;
    let head = records[..head_lines].join("\n");
    let tail = records[records.len() - tail_lines..].join("\n");
    let dropped_bytes = records.join("\n").len() - head.len() - tail.len();
    format!(
        "{head}\n\n{}\n\n{tail}",
        marker(records.len() - MODEL_LINES, dropped_bytes)
    )
}

#[test_case(false ; "multiline")]
#[test_case(true ; "single_line")]
fn bash_bounds_large_output_before_lua(single_line: bool) {
    let (_host, ctx) = setup(ToolOutputMode::Compact);
    let output = run_long(&ctx, large_output_command(single_line));
    let head = "0".repeat(HEAD_BYTES);
    let expected = if single_line {
        let dropped_bytes = STREAM_BYTES + FINAL_MARKER.len() - HEAD_BYTES;
        format!("{head}\n\n{}", marker(0, dropped_bytes))
    } else {
        let dropped_bytes = STREAM_BYTES - HEAD_BYTES;
        format!(
            "{head}\n\n{}\n\n{FINAL_MARKER}",
            marker(CHUNK_COUNT - 1, dropped_bytes)
        )
    };
    assert_eq!(output, expected);
    assert_host_recovers(&ctx);
}

#[test]
fn bash_full_mode_fails_once_output_passes_the_ceiling() {
    let (_host, ctx) = setup(ToolOutputMode::Full);
    let line_bytes = FULL_OUTPUT_MAX_BYTES + 1;
    let command =
        format!("head -c {line_bytes} /dev/zero | tr '\\0' x; echo; printf {FINAL_MARKER}");
    let output = run(&ctx, json!({ "command": command, "timeout": TIMEOUT_SECS }))
        .expect_err("a full-mode cut must be an error");
    let head_bytes = FULL_OUTPUT_MAX_BYTES.div_ceil(2);
    let dropped_bytes = line_bytes + 1 - head_bytes;
    let expected = format!(
        "{}\n\n{}\n\n{FINAL_MARKER}\n{FULL_LIMIT_NOTE}",
        "x".repeat(head_bytes),
        marker(0, dropped_bytes)
    );
    assert!(
        output == expected,
        "got {}, want {}",
        squeeze(&output),
        squeeze(&expected)
    );
    assert_host_recovers(&ctx);
}

#[test_case(0, false ; "stdout_success")]
#[test_case(FAILURE_CODE, false ; "stdout_failure")]
#[test_case(0, true ; "stderr_success")]
#[test_case(FAILURE_CODE, true ; "stderr_failure")]
fn bash_keeps_both_ends_and_exit_status(exit_code: usize, stderr: bool) {
    let (_host, ctx) = setup(ToolOutputMode::Compact);
    let redirect = if stderr { " >&2" } else { "" };
    let command = format!(
        "awk 'BEGIN {{ for (i = 0; i < {RECORD_COUNT}; i++) print \"record-\" i }}'{redirect}; exit {exit_code}"
    );
    let output = run(&ctx, json!({ "command": command }));
    let expected = both_ends(&records());
    if exit_code == 0 {
        assert_eq!(output.unwrap(), expected);
    } else {
        assert_eq!(output.unwrap_err(), format!("{expected}\n{FAILURE_STATUS}"));
    }
}
