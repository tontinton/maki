#![cfg(unix)]

use std::sync::Arc;

use maki_agent::AgentMode;
use maki_agent::agent::tool_dispatch;
use maki_agent::tools::interpreter_bridge;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{CallOrigin, ToolContext, ToolRegistry};
use maki_config::{DEFAULT_BUILTINS, PluginFileConfig, PluginsConfig};
use maki_lua::PluginHost;
use serde_json::{Value, json};

const MODEL_BYTES: usize = 1024;
const MODEL_LINES: usize = 8;
const STREAM_BYTES: usize = 600 * 1024 * 1024;
const CHUNK_BYTES: usize = 8 * 1024;
const CHUNK_COUNT: usize = STREAM_BYTES / CHUNK_BYTES;
const RECORD_COUNT: usize = MODEL_LINES * 3;
const TAIL_LINES: usize = 2;
const OVERSIZED_TAIL_LINES: usize = MODEL_LINES * 2;
const TIMEOUT_SECS: usize = 120;
const CALL_ID: &str = "output-capture-test";
const FINAL_MARKER: &str = "final-output-marker";
const RECOVERY_MARKER: &str = "subsequent-command-completed";
const TRUNCATION_PREFIX: &str = "[truncated ";
const FAILURE_CODE: usize = 7;
const FAILURE_STATUS: &str = "Exit code: 7";

fn setup() -> (PluginHost, ToolContext) {
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
    ctx.config.rtk = false;
    ctx.config.max_output_bytes = MODEL_BYTES;
    ctx.config.max_output_lines = MODEL_LINES;
    (host, ctx)
}

fn run(ctx: &ToolContext, input: Value) -> Result<String, String> {
    let done = smol::block_on(tool_dispatch::run(
        CALL_ID.into(),
        "bash",
        &input,
        ctx,
        CallOrigin::Model,
    ));
    interpreter_bridge::flatten(&done)
}

fn large_output_command(single_line: bool) -> String {
    let newline = if single_line { "" } else { "\\n" };
    let chunk_bytes = CHUNK_BYTES - usize::from(!single_line);
    format!(
        "awk 'BEGIN {{ chunk = sprintf(\"%0{chunk_bytes}d\", 0); for (i = 0; i < {CHUNK_COUNT}; i++) printf \"%s{newline}\", chunk; printf \"%s\", \"{FINAL_MARKER}\" }}'"
    )
}

#[test_case::test_case(false ; "multiline")]
#[test_case::test_case(true ; "single_line")]
fn bash_bounds_large_output_before_lua(single_line: bool) {
    let (_host, ctx) = setup();
    let output = run(
        &ctx,
        json!({ "command": large_output_command(single_line), "timeout": TIMEOUT_SECS }),
    )
    .expect("large command must finish without exhausting the Lua heap");
    let dropped_bytes = STREAM_BYTES + FINAL_MARKER.len() - MODEL_BYTES;
    assert_eq!(
        output,
        format!(
            "{}\n\n{TRUNCATION_PREFIX}{dropped_bytes} bytes]",
            "0".repeat(MODEL_BYTES)
        )
    );
    assert_eq!(
        run(
            &ctx,
            json!({ "command": format!("printf '{RECOVERY_MARKER}'") }),
        )
        .expect("the same Lua host must remain usable after large output"),
        RECOVERY_MARKER
    );
}

#[test_case::test_case(0, false, TAIL_LINES ; "stdout_success")]
#[test_case::test_case(FAILURE_CODE, false, TAIL_LINES ; "stdout_failure")]
#[test_case::test_case(0, true, TAIL_LINES ; "stderr_success")]
#[test_case::test_case(FAILURE_CODE, true, TAIL_LINES ; "stderr_failure")]
#[test_case::test_case(0, false, OVERSIZED_TAIL_LINES ; "limited_stdout_success")]
#[test_case::test_case(FAILURE_CODE, false, OVERSIZED_TAIL_LINES ; "limited_stdout_failure")]
#[test_case::test_case(0, true, OVERSIZED_TAIL_LINES ; "limited_stderr_success")]
#[test_case::test_case(FAILURE_CODE, true, OVERSIZED_TAIL_LINES ; "limited_stderr_failure")]
fn bash_preserves_tail_and_exit_status(exit_code: usize, stderr: bool, tail: usize) {
    let (_host, ctx) = setup();
    let redirect = if stderr { " >&2" } else { "" };
    let command = format!(
        "awk 'BEGIN {{ for (i = 0; i < {RECORD_COUNT}; i++) print \"record-\" i }}'{redirect}; exit {exit_code}"
    );
    let output = run(&ctx, json!({ "command": command, "tail": tail }));
    let mut expected = (RECORD_COUNT - tail.min(MODEL_LINES)..RECORD_COUNT)
        .map(|i| format!("record-{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    if tail > MODEL_LINES {
        let dropped = tail.min(RECORD_COUNT) - MODEL_LINES;
        expected.push_str(&format!(
            "\n\n[truncated {dropped} lines from requested tail]"
        ));
    }
    if exit_code == 0 {
        assert_eq!(output.unwrap(), expected);
    } else {
        assert_eq!(output.unwrap_err(), format!("{expected}\n{FAILURE_STATUS}"));
    }
}

#[test]
fn bash_applies_model_line_limit_during_capture() {
    let (_host, ctx) = setup();
    let command =
        format!("awk 'BEGIN {{ for (i = 0; i < {RECORD_COUNT}; i++) print \"record-\" i }}'");
    let output = run(&ctx, json!({ "command": command })).unwrap();
    let records = (0..RECORD_COUNT)
        .map(|i| format!("record-{i}"))
        .collect::<Vec<_>>();
    let prefix = records[..MODEL_LINES].join("\n");
    let dropped_bytes = records.join("\n").len() - prefix.len();
    assert_eq!(
        output,
        format!("{prefix}\n\n{TRUNCATION_PREFIX}{dropped_bytes} bytes]")
    );
}
