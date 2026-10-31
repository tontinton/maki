#![cfg(unix)]
//! The bundled bash plugin moved to the background through the runtime's hook
//! path: the receipt, the output log, the bounded tail and the timeout option.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use common::{Chat, Gate, WAIT, done_result, isolated_state, poll_until, request_move};
use maki_agent::agent::tool_dispatch;
use maki_agent::template::Vars;
use maki_agent::tools::{
    CallOrigin, DescriptionContext, ToolAudience, ToolFilter, ToolOutputMode, ToolRegistry,
};
use maki_agent::{
    AgentEvent, AgentSeed, CallScope, CancelToken, CancelTrigger, Envelope, EventSender,
    SessionMailbox, SharedBuf,
};
use maki_config::{PluginFileConfig, PluginsConfig};
use maki_lua::test_support::runtime;
use maki_lua::{EventHandle, MoveOutcome, PluginHost};
use maki_storage::id::MakiId;
use maki_storage::paths::{session_task_logs_dir, state_dir};
use serde_json::{Value, json};
use tempfile::TempDir;
use test_case::test_case;

const RECEIPT_HEAD: &str = "Running in the background.";
const LOG_LABEL: &str = "Full output: ";
/// The runtime's move note follows the plugin's receipt after a blank line.
const MOVE_NOTE_SEPARATOR: &str = "\n\n";
const ROTATED_SUFFIX: &str = ".1";
const LOG_CAP_BYTES: u64 = 2048;
const TAIL_LINES: usize = 5;
const BASH_TOOL: &str = "bash";
const BATCH_TOOL: &str = "batch";
const TASK_TOOL: &str = "task";
const TASK_STOP_TOOL: &str = "task_stop";
const BACKGROUND_PARAM: &str = "background";
/// Long enough for the handler to reach `jobstart` on a loaded CI box.
const TIMEOUT_SECS: u64 = 5;
/// The last line printed before the gate. The command text is part of the
/// live buf, so the marker is computed by the shell and never spelled out in it.
const READY_COMMAND: &str = "echo ready-$((40+2))";
const READY_LINE: &str = "ready-42";
const FAILING_COMMAND: &str = "exit 3";
const DESCRIPTION: &str = "Check the build";
const FINISHED_LATE: &str = "finished-late";
// Mirrors of the runtime's wording.
const MOVED_AS_ASKED: &str = "Started in the background as asked";
const EARLIER_IN_LOG: &str = "earlier lines are in the full output log";
const EARLIER_DROPPED: &str = "earlier lines dropped]";
const TRUNCATED: &str = "[truncated ";
/// Past the default reply budget, under the line cap bash gives its job.
const LONG_LINE_BYTES: usize = 64 * 1024;
const EARLY_LINES: usize = 10;
const LATE_LINES: usize = 300;
/// Once the reply is cut, the newest lines it keeps are the second half of
/// its line budget, and they seed the tail on the move.
const KEPT_TAIL_LINES: usize = TAIL_LINES / 2;
/// Lines of 49 bytes that cross `LOG_CAP_BYTES` once.
const PADDED_LINES: usize = 60;

fn builtins_host(opts: &Value) -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let mut bash = PluginFileConfig::default();
    bash.opts
        .extend(opts.as_object().expect("opts are an object").clone());
    let mut task = PluginFileConfig::default();
    task.opts
        .insert("allow_background".into(), Value::Bool(true));
    let config = PluginsConfig::from_plugins(HashMap::from([
        ("bash".to_owned(), bash),
        (TASK_TOOL.to_owned(), task),
    ]));
    host.load_builtins(&config).unwrap();
    (reg, host)
}

/// One bash call in a live chat, executing on its own thread so the test can
/// ask for the move from outside, the way Ctrl+B does.
struct Run {
    handle: EventHandle,
    call_id: String,
    session: MakiId,
    mailbox: SessionMailbox,
    events: flume::Receiver<Envelope>,
    result: flume::Receiver<Result<String, String>>,
    live: Option<Arc<SharedBuf>>,
    gate: Gate,
    finish: flume::Sender<()>,
    thread: Option<JoinHandle<()>>,
    _seed: Arc<AgentSeed>,
    _run_trigger: CancelTrigger,
    _state: TempDir,
}

impl Run {
    fn start(opts: Value, timeout: Option<u64>, before: &str, after: &str) -> Self {
        let gate = Gate::closed();
        let mut input = json!({ "command": gate.command(before, after) });
        if let Some(secs) = timeout {
            input["timeout"] = json!(secs);
        }
        Self::launch(opts, BASH_TOOL, input, gate)
    }

    /// The same command as the only child of a batch, the way a model runs
    /// bash next to other calls.
    fn start_in_batch(before: &str, after: &str) -> Self {
        let gate = Gate::closed();
        let command = gate.command(before, after);
        let input = json!({
            "tool_calls": [{ "tool": BASH_TOOL, "parameters": { "command": command } }]
        });
        Self::launch(json!({}), BATCH_TOOL, input, gate)
    }

    /// Isolates the state directory before the call's thread starts.
    fn launch(opts: Value, tool: &str, input: Value, gate: Gate) -> Self {
        let state = isolated_state();
        let Chat {
            id: session,
            mailbox,
            seed,
        } = Chat::live();
        let call_id = format!("toolu_{}", MakiId::generate());
        let (events_tx, events) = flume::unbounded();
        let (run_trigger, run_token) = CancelToken::new();
        let (handle_tx, handle_rx) = flume::bounded(1);
        let (result_tx, result) = flume::bounded(1);
        let (finish, finished) = flume::bounded::<()>(1);

        let tool = tool.to_owned();
        let thread = thread::spawn({
            let seed = Arc::clone(&seed);
            let mailbox = mailbox.clone();
            let call_id = call_id.clone();
            move || {
                let (reg, host) = builtins_host(&opts);
                handle_tx.send(host.event_handle()).unwrap();
                let scope = Arc::new(CallScope::new(call_id.clone(), &run_token));
                let mut ctx = seed.tool_context(scope.cancel().clone());
                ctx.event_tx = EventSender::new(events_tx, 0);
                ctx.tool_use_id = Some(call_id.clone());
                ctx.call = Some(scope);
                ctx.mailbox = Some(mailbox);
                ctx.output_mode = ToolOutputMode::Compact;
                ctx.config.rtk = false;
                ctx.registry = Arc::clone(&reg);
                // A batch dispatches its children through the permission
                // check, and this chat has nobody to answer a prompt.
                if !ctx.permissions.is_yolo() {
                    ctx.permissions.toggle_yolo();
                }
                let done = runtime().block_on(tool_dispatch::run(
                    call_id,
                    &tool,
                    &input,
                    &ctx,
                    CallOrigin::Model,
                ));
                result_tx.send(done_result(&done)).ok();
                // The final reply is delivered by the host, which has to
                // outlive the call.
                finished.recv().ok();
                drop(host);
            }
        });

        Self {
            handle: handle_rx.recv_timeout(WAIT).expect("host starts"),
            call_id,
            session,
            mailbox,
            events,
            result,
            live: None,
            gate,
            finish,
            thread: Some(thread),
            _seed: seed,
            _run_trigger: run_trigger,
            _state: state,
        }
    }

    fn log_path(&self) -> PathBuf {
        session_task_logs_dir(&state_dir().unwrap(), self.session)
            .join(format!("{}.log", self.call_id))
    }

    fn release(&self) {
        self.gate.release();
    }

    /// Output shows in the live buf in order, so seeing the last line printed
    /// before the gate means the plugin has them all.
    fn wait_for_output(&mut self, needle: &str) {
        poll_until("bash never showed the expected output", || {
            if self.live.is_none() {
                self.live = self.events.drain().find_map(|env| match env.event {
                    AgentEvent::LiveToolBuf { id, body } if id == self.call_id => Some(body),
                    _ => None,
                });
            }
            self.live
                .as_ref()
                .is_some_and(|buf| buf.take().text().contains(needle))
                .then_some(())
        });
    }

    fn request_move(&self) -> MoveOutcome {
        request_move(&self.handle, &self.call_id)
    }

    fn move_to_background(&self) -> String {
        assert_eq!(self.request_move(), MoveOutcome::Moved);
        self.reply().expect("a receipt is a success")
    }

    fn reply(&self) -> Result<String, String> {
        self.result.recv_timeout(WAIT).expect("the call replies")
    }

    fn final_notice(&self) -> String {
        poll_until("the final notice never arrived", || {
            self.mailbox
                .drain()
                .iter()
                .find_map(|notice| notice.first_text_content().map(str::to_owned))
        })
    }

    fn log(&self) -> String {
        fs::read_to_string(self.log_path()).unwrap()
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.release();
        self.finish.send(()).ok();
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

fn progress_line(run: &Run) -> String {
    format!("{LOG_LABEL}{}", run.log_path().display())
}

#[test]
fn log_keeps_growing_after_the_move_and_the_final_reply_names_it() {
    let mut run = Run::start(
        json!({}),
        None,
        &format!("echo before; {READY_COMMAND}"),
        "echo after-1; echo after-2",
    );
    run.wait_for_output(READY_LINE);

    let receipt = run.move_to_background();

    let expected = format!(
        "{RECEIPT_HEAD} Last 2 lines:\nbefore\n{READY_LINE}\n{}",
        progress_line(&run)
    );
    assert_eq!(
        receipt.split_once(MOVE_NOTE_SEPARATOR).map(|(own, _)| own),
        Some(expected.as_str()),
        "{receipt}"
    );
    assert_eq!(run.log(), format!("before\n{READY_LINE}\n"));

    run.release();
    let notice = run.final_notice();

    assert!(
        notice.contains(&format!("before\n{READY_LINE}\nafter-1\nafter-2")),
        "{notice}"
    );
    assert!(notice.contains(&progress_line(&run)), "{notice}");
    assert_eq!(
        run.log(),
        format!("before\n{READY_LINE}\nafter-1\nafter-2\n")
    );
}

/// The model asked up front, so the receipt answers at once and the command
/// runs on behind it.
#[test]
fn the_background_param_moves_the_command_at_once() {
    let gate = Gate::closed();
    let mut input =
        json!({ "command": gate.command("echo started", &format!("echo {FINISHED_LATE}")) });
    input[BACKGROUND_PARAM] = json!(true);
    let run = Run::launch(json!({}), BASH_TOOL, input, gate);

    let receipt = run.reply().expect("a receipt is a success");

    assert!(receipt.starts_with(RECEIPT_HEAD), "{receipt}");
    assert!(receipt.contains(&progress_line(&run)), "{receipt}");
    assert!(receipt.contains(MOVED_AS_ASKED), "{receipt}");
    run.release();
    let notice = run.final_notice();
    assert!(notice.contains(FINISHED_LATE), "{notice}");
}

/// The chat row a call moved out of shows only the head of its buf, so the
/// head must be the command, nothing more and nothing less.
#[test]
fn a_moved_command_is_the_head_of_its_buf() {
    let gate = Gate::closed();
    let command = gate.command(READY_COMMAND, "true");
    let mut input = json!({ "command": command });
    input[BACKGROUND_PARAM] = json!(true);
    let mut run = Run::launch(json!({}), BASH_TOOL, input, gate);
    run.reply().expect("a receipt is a success");

    run.wait_for_output(READY_LINE);

    let snapshot = run.live.as_ref().unwrap().take();
    assert_eq!(snapshot.head_only().map(|head| head.text()), Some(command));
}

#[test]
fn memory_stays_bounded_while_the_log_keeps_everything() {
    let mut run = Run::start(
        json!({ "max_output_lines": TAIL_LINES }),
        None,
        &format!("for i in $(seq 1 {EARLY_LINES}); do echo early-$i; done; {READY_COMMAND}"),
        &format!("for i in $(seq 1 {LATE_LINES}); do echo late-$i; done"),
    );
    run.wait_for_output(READY_LINE);

    let receipt = run.move_to_background();
    assert!(
        receipt.contains(&format!(
            "Last {KEPT_TAIL_LINES} lines:\nearly-10\n{READY_LINE}\n"
        )),
        "{receipt}"
    );

    run.release();
    let notice = run.final_notice();

    assert!(
        notice.contains("late-296\nlate-297\nlate-298\nlate-299\nlate-300"),
        "{notice}"
    );
    for dropped in ["late-295", "late-100", "early-10"] {
        assert!(!notice.contains(dropped), "{dropped} in {notice}");
    }
    let total_lines = EARLY_LINES + 1 + LATE_LINES;
    let marker = format!("[{} {EARLIER_IN_LOG}]", total_lines - TAIL_LINES);
    assert!(notice.contains(&marker), "{notice}");
    let log = run.log();
    assert_eq!(log.lines().count(), total_lines);
    assert!(log.starts_with("early-1\n") && log.ends_with("late-300\n"));
}

/// The model can read what the cut dropped instead of running the command
/// again, long lines included.
#[test]
fn a_cut_foreground_reply_names_a_log_with_every_line() {
    let long_line = "0".repeat(LONG_LINE_BYTES);
    let run = Run::start(
        json!({ "max_output_lines": TAIL_LINES }),
        None,
        &format!("for i in $(seq 1 {LATE_LINES}); do echo late-$i; done; echo {long_line}"),
        "true",
    );
    run.release();

    let reply = run.reply().expect("the command succeeds");

    assert!(reply.contains(TRUNCATED), "{reply}");
    assert!(reply.ends_with(&progress_line(&run)), "{reply}");
    let every_line: String = (1..=LATE_LINES)
        .map(|i| format!("late-{i}\n"))
        .chain([format!("{long_line}\n")])
        .collect();
    assert_eq!(run.log(), every_line);
}

/// Output that fits the reply leaves no log behind.
#[test]
fn a_foreground_reply_that_fits_has_no_log() {
    let run = Run::start(json!({}), None, "echo only", "true");
    run.release();

    assert_eq!(run.reply().as_deref(), Ok("only"));
    assert!(!run.log_path().exists());
}

/// The reply's note on the lines its tail dropped points at both files,
/// since the oldest of them may already be gone with an earlier rotation.
#[test]
fn log_rotates_at_the_cap_and_the_reply_names_the_rotated_copy() {
    let mut run = Run::start(
        json!({ "background_log_bytes": LOG_CAP_BYTES, "max_output_lines": TAIL_LINES }),
        None,
        &format!("echo start; {READY_COMMAND}"),
        &format!("for i in $(seq 1 {PADDED_LINES}); do printf '%048d\\n' $i; done"),
    );
    run.wait_for_output(READY_LINE);
    run.move_to_background();
    run.release();
    let notice = run.final_notice();

    let expected: String = ["start".to_owned(), READY_LINE.to_owned()]
        .into_iter()
        .chain((1..=PADDED_LINES).map(|i| format!("{i:048}")))
        .map(|line| line + "\n")
        .collect();
    assert!(
        expected.len() as u64 > LOG_CAP_BYTES && (expected.len() as u64) < 2 * LOG_CAP_BYTES,
        "the output must cross the cap exactly once"
    );
    let rotated = format!("{}{ROTATED_SUFFIX}", run.log_path().display());
    assert_eq!(fs::read_to_string(&rotated).unwrap() + &run.log(), expected);
    assert!((run.log().len() as u64) < LOG_CAP_BYTES);
    let marker = format!("{EARLIER_IN_LOG} or {rotated}; the oldest may be gone]");
    assert!(notice.contains(&marker), "{notice}");
}

#[test]
fn a_log_that_stopped_taking_writes_does_not_claim_to_hold_the_earlier_lines() {
    let mut run = Run::start(
        json!({ "max_output_lines": TAIL_LINES }),
        None,
        READY_COMMAND,
        "for i in $(seq 1 30); do echo late-$i; done",
    );
    run.wait_for_output(READY_LINE);
    run.move_to_background();
    let log_path = run.log_path();
    fs::remove_file(&log_path).unwrap();
    fs::create_dir(&log_path).unwrap();
    run.release();
    let notice = run.final_notice();

    assert!(notice.contains(EARLIER_DROPPED), "{notice}");
    assert!(!notice.contains(EARLIER_IN_LOG), "{notice}");
}

fn offered_tools(audience: ToolAudience, background: bool) -> Value {
    let (reg, _host) = builtins_host(&json!({}));
    let ctx = DescriptionContext {
        filter: &ToolFilter::All,
        audience,
        workflow: false,
        mcp: false,
    };
    reg.definitions_with_background(&Vars::new(), &ctx, false, background)
}

fn tool_def<'a>(defs: &'a Value, name: &str) -> Option<&'a Value> {
    defs.as_array()?.iter().find(|def| def["name"] == name)
}

#[test_case(ToolAudience::MAIN, true, true, true, true ; "main_with_background_on")]
#[test_case(ToolAudience::MAIN, false, false, false, true ; "main_with_background_off")]
#[test_case(ToolAudience::GENERAL_SUB, true, false, false, false ; "subagent_with_background_on")]
fn the_background_param_and_task_stop_are_offered_by_audience_and_mode(
    audience: ToolAudience,
    background: bool,
    offers_param: bool,
    offers_task_stop: bool,
    offers_task: bool,
) {
    let defs = offered_tools(audience, background);

    let bash = tool_def(&defs, BASH_TOOL).expect("bash is offered to every audience");
    assert!(bash["input_schema"]["properties"]["command"].is_object());
    let task = tool_def(&defs, TASK_TOOL);
    assert_eq!(task.is_some(), offers_task, "{defs}");
    for tool in [Some(bash), task].into_iter().flatten() {
        assert_eq!(
            tool["input_schema"]["properties"]
                .get(BACKGROUND_PARAM)
                .is_some(),
            offers_param,
            "{tool}"
        );
    }
    assert_eq!(
        tool_def(&defs, TASK_STOP_TOOL).is_some(),
        offers_task_stop,
        "{defs}"
    );
}

/// A bash inside a moved batch never runs its own hook, so it switches to the
/// bounded tail and opens its log on the first output after the move.
#[test]
fn a_bash_in_a_moved_batch_logs_the_output_it_prints_after_the_move() {
    let run = Run::start_in_batch(
        &format!("echo before; {READY_COMMAND}"),
        "echo after-1; echo after-2",
    );
    poll_until("the batch never moved", || {
        (run.request_move() == MoveOutcome::Moved).then_some(())
    });
    run.reply().expect("a receipt is a success");

    run.release();
    let notice = run.final_notice();

    let logs = session_task_logs_dir(&state_dir().unwrap(), run.session);
    let log_path = notice
        .split_once(LOG_LABEL)
        .and_then(|(_, rest)| rest.lines().next())
        .unwrap_or_else(|| panic!("no log path in {notice}"));
    assert!(Path::new(log_path).starts_with(&logs), "{log_path}");
    assert_eq!(
        fs::read_to_string(log_path).unwrap(),
        format!("before\n{READY_LINE}\nafter-1\nafter-2\n")
    );
    assert!(notice.contains("after-1\nafter-2"), "{notice}");
}

#[test]
fn a_timeout_kills_the_command_by_default() {
    let run = Run::start(json!({}), Some(TIMEOUT_SECS), "echo started", "true");

    let err = run.reply().expect_err("a timeout is an error reply");

    assert!(
        err.contains(&format!("timed out after {TIMEOUT_SECS}s")),
        "{err}"
    );
    assert!(!run.log_path().exists());
}

#[test]
fn a_timeout_moves_the_command_when_background_on_timeout_is_set() {
    let run = Run::start(
        json!({ "background_on_timeout": true }),
        Some(TIMEOUT_SECS),
        "echo started",
        &format!("echo {FINISHED_LATE}"),
    );

    let receipt = run.reply().expect("the move is a success");
    assert!(receipt.contains(RECEIPT_HEAD), "{receipt}");

    run.release();
    let notice = run.final_notice();
    assert!(notice.contains(FINISHED_LATE), "{notice}");
    assert_eq!(run.log(), format!("started\n{FINISHED_LATE}\n"));
}

/// The timeout sits beside the header as an annotation, so the summary that
/// names the call in `/tasks`, a rewind and a background notice is the label
/// alone.
#[test]
fn the_timeout_annotates_the_header_and_stays_out_of_the_summary() {
    let input = json!({ "command": "true", "description": DESCRIPTION, "timeout": TIMEOUT_SECS });
    let run = Run::launch(json!({}), BASH_TOOL, input, Gate::closed());

    let start = poll_until("bash never started", || {
        run.events.try_iter().find_map(|env| match env.event {
            AgentEvent::ToolStart(start) => Some(start),
            _ => None,
        })
    });

    assert_eq!(start.summary, DESCRIPTION);
    assert_eq!(start.annotation, Some(format!("{TIMEOUT_SECS}s timeout")));
}

/// The UI repeats an error reply under a view that lacks the reply's last
/// line, so the view of a moved command must end on the log line its final
/// reply ends on, or a stopped or failed command shows its output twice.
#[test_case(false ; "failed")]
#[test_case(true ; "stopped")]
fn the_view_ends_on_the_line_the_final_reply_ends_on(stop: bool) {
    let mut run = Run::start(json!({}), None, READY_COMMAND, FAILING_COMMAND);
    run.wait_for_output(READY_LINE);
    run.move_to_background();

    if stop {
        run.handle.cancel_plugin_session(&run.call_id);
    } else {
        run.release();
    }
    let notice = run.final_notice();

    let log_line = progress_line(&run);
    assert_eq!(notice.lines().last(), Some(log_line.as_str()), "{notice}");
    let view = run.live.as_ref().expect("the live buf").take().text();
    assert_eq!(view.lines().last(), Some(log_line.as_str()), "{view}");
}
