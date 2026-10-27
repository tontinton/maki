#![cfg(unix)]
//! The bundled bash plugin moved to the background through the runtime's hook
//! path: the receipt, the output log, the bounded tail and the timeout option.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use maki_agent::tools::test_support::stub_seed;
use maki_agent::tools::{ToolOutputMode, ToolRegistry};
use maki_agent::{
    AgentEvent, AgentSeed, CallScope, CancelToken, CancelTrigger, Envelope, EventSender,
    SessionMailbox, SharedBuf, ToolOutput,
};
use maki_config::{PluginFileConfig, PluginsConfig};
use maki_lua::{EventHandle, MoveOutcome, PluginHost};
use maki_storage::id::MakiId;
use maki_storage::paths::{session_task_logs_dir, state_dir};
use serde_json::{Value, json};
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const GATE_FILE: &str = "go";
const GATE_POLL_SECS: &str = "0.02";
const RECEIPT_HEAD: &str = "Running in the background.";
const PROGRESS_LABEL: &str = "Full output (read it to check progress): ";
const FINAL_LABEL: &str = "Full output: ";
const ROTATED_SUFFIX: &str = ".1";
const LOG_CAP_BYTES: u64 = 2048;
const TAIL_LINES: usize = 5;
const BASH_TOOL: &str = "bash";
const BATCH_TOOL: &str = "batch";
const STATE_ENV_VARS: [&str; 2] = ["HOME", "XDG_STATE_HOME"];
/// Long enough for the handler to reach `jobstart` on a loaded CI box.
const TIMEOUT_SECS: u64 = 5;
const TIMED_OUT_MARKER: &str = "timed out after 5s";
/// The last line printed before the gate. The command text is part of the
/// live buf, so the marker is computed by the shell and never spelled out in it.
const READY_COMMAND: &str = "echo ready-$((40+2))";
const READY_LINE: &str = "ready-42";

fn poll_until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(got) = check() {
            return got;
        }
        assert!(Instant::now() < deadline, "{what}");
        thread::sleep(POLL_INTERVAL);
    }
}

fn builtins_host(opts: &Value) -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let mut bash = PluginFileConfig::default();
    bash.opts
        .extend(opts.as_object().expect("opts are an object").clone());
    let config = PluginsConfig::from_plugins(HashMap::from([("bash".to_owned(), bash)]));
    host.load_builtins(&config).unwrap();
    (reg, host)
}

/// Points the state directory at a throwaway tree, so task logs never land in
/// the user's. Dropping the tree is the cleanup.
fn isolated_state() -> TempDir {
    let dir = TempDir::new().unwrap();
    for var in STATE_ENV_VARS {
        // SAFETY: setting a variable is only sound while no other thread reads
        // the environment. Every test runs in its own process under nextest
        // and calls this before it starts a thread.
        unsafe { std::env::set_var(var, dir.path()) };
    }
    dir
}

fn gated_command(gate_file: &Path, before: &str, after: &str) -> String {
    format!(
        "{before}; while [ ! -e '{}' ]; do sleep {GATE_POLL_SECS}; done; {after}",
        gate_file.display()
    )
}

fn plain_text(output: ToolOutput) -> String {
    match output {
        ToolOutput::Plain(s) | ToolOutput::Markdown(s) => s.text,
        other => panic!("unexpected output: {other:?}"),
    }
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
    gate: TempDir,
    finish: flume::Sender<()>,
    thread: Option<JoinHandle<()>>,
    _seed: Arc<AgentSeed>,
    _run_trigger: CancelTrigger,
    _state: TempDir,
}

impl Run {
    /// Prints `before`, then waits for `release`, then runs `after`.
    fn start(opts: Value, timeout: Option<u64>, before: &str, after: &str) -> Self {
        let state = isolated_state();
        let gate = TempDir::new().unwrap();
        let command = gated_command(&gate.path().join(GATE_FILE), before, after);
        let mut input = json!({ "command": command });
        if let Some(secs) = timeout {
            input["timeout"] = json!(secs);
        }
        Self::launch(state, opts, BASH_TOOL, input, gate)
    }

    /// The same command as the only child of a batch, the way a model runs
    /// bash next to other calls.
    fn start_in_batch(before: &str, after: &str) -> Self {
        let state = isolated_state();
        let gate = TempDir::new().unwrap();
        let command = gated_command(&gate.path().join(GATE_FILE), before, after);
        let input = json!({
            "tool_calls": [{ "tool": BASH_TOOL, "parameters": { "command": command } }]
        });
        Self::launch(state, json!({}), BATCH_TOOL, input, gate)
    }

    fn launch(state: TempDir, opts: Value, tool: &str, input: Value, gate: TempDir) -> Self {
        let session = MakiId::generate();
        let mailbox = SessionMailbox::register(session);
        let seed = mailbox.publish_seed(stub_seed(session));
        let call_id = format!("toolu_{}", MakiId::generate());
        let (events_tx, events) = flume::unbounded();
        let (run_trigger, run_token) = CancelToken::new();
        let (handle_tx, handle_rx) = flume::bounded(1);
        let (result_tx, result) = flume::bounded(1);
        let (finish, finished) = flume::bounded::<()>(1);

        let tool = tool.to_owned();
        let thread = thread::spawn({
            let seed = Arc::clone(&seed);
            let call_id = call_id.clone();
            move || {
                let (reg, host) = builtins_host(&opts);
                handle_tx.send(host.event_handle()).unwrap();
                let scope = Arc::new(CallScope::new(call_id.clone(), &run_token));
                let mut ctx = seed.tool_context(scope.cancel().clone());
                ctx.event_tx = EventSender::new(events_tx, 0);
                ctx.tool_use_id = Some(call_id);
                ctx.call = Some(scope);
                ctx.output_mode = ToolOutputMode::Compact;
                ctx.config.rtk = false;
                ctx.registry = Arc::clone(&reg);
                // A batch dispatches its children through the permission
                // check, and this chat has nobody to answer a prompt.
                if !ctx.permissions.is_yolo() {
                    ctx.permissions.toggle_yolo();
                }
                let inv = reg
                    .get(&tool)
                    .unwrap_or_else(|| panic!("{tool} registered"))
                    .tool
                    .parse(&input)
                    .expect("parse failed");
                let result = smol::block_on(inv.execute(&ctx)).output.map(plain_text);
                result_tx.send(result).ok();
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
        fs::write(self.gate.path().join(GATE_FILE), "").unwrap();
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
        self.handle
            .request_background(self.call_id.clone())
            .recv_timeout(WAIT)
            .expect("the hook answers")
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
    format!("{PROGRESS_LABEL}{}", run.log_path().display())
}

#[test]
fn receipt_has_the_tail_and_the_log_path() {
    let mut run = Run::start(
        json!({}),
        None,
        &format!("echo one; echo two; {READY_COMMAND}"),
        "true",
    );
    run.wait_for_output(READY_LINE);

    let receipt = run.move_to_background();

    let expected = format!(
        "{RECEIPT_HEAD} Last 3 lines:\none\ntwo\n{READY_LINE}\n{}",
        progress_line(&run)
    );
    assert!(receipt.contains(&expected), "{receipt}");
    assert_eq!(run.log(), format!("one\ntwo\n{READY_LINE}\n"));
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
    run.move_to_background();
    assert_eq!(run.log(), format!("before\n{READY_LINE}\n"));

    run.release();
    let notice = run.final_notice();

    assert!(
        notice.contains(&format!("before\n{READY_LINE}\nafter-1\nafter-2")),
        "{notice}"
    );
    assert!(
        notice.contains(&format!("{FINAL_LABEL}{}", run.log_path().display())),
        "{notice}"
    );
    assert_eq!(
        run.log(),
        format!("before\n{READY_LINE}\nafter-1\nafter-2\n")
    );
}

#[test]
fn memory_stays_bounded_while_the_log_keeps_everything() {
    let mut run = Run::start(
        json!({ "max_output_lines": TAIL_LINES }),
        None,
        &format!("for i in $(seq 1 10); do echo early-$i; done; {READY_COMMAND}"),
        "for i in $(seq 1 300); do echo late-$i; done",
    );
    run.wait_for_output(READY_LINE);

    let receipt = run.move_to_background();
    assert!(
        receipt.contains(&format!(
            "Last {TAIL_LINES} lines:\nearly-7\nearly-8\nearly-9\nearly-10\n{READY_LINE}\n"
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
    let marker = format!(
        "[{} earlier lines are in the full output log]",
        311 - TAIL_LINES
    );
    assert!(notice.contains(&marker), "{notice}");
    let log = run.log();
    assert_eq!(log.lines().count(), 311);
    assert!(log.starts_with("early-1\n") && log.ends_with("late-300\n"));
}

#[test]
fn log_rotates_at_the_cap() {
    let mut run = Run::start(
        json!({ "background_log_bytes": LOG_CAP_BYTES }),
        None,
        &format!("echo start; {READY_COMMAND}"),
        "for i in $(seq 1 60); do printf '%048d\\n' $i; done",
    );
    run.wait_for_output(READY_LINE);
    run.move_to_background();
    run.release();
    run.final_notice();

    let expected: String = ["start".to_owned(), READY_LINE.to_owned()]
        .into_iter()
        .chain((1..=60).map(|i| format!("{i:048}")))
        .map(|line| line + "\n")
        .collect();
    let rotated = PathBuf::from(format!("{}{ROTATED_SUFFIX}", run.log_path().display()));
    assert!(
        expected.len() as u64 > LOG_CAP_BYTES && (expected.len() as u64) < 2 * LOG_CAP_BYTES,
        "the output must cross the cap exactly once"
    );
    assert_eq!(fs::read_to_string(&rotated).unwrap() + &run.log(), expected);
    assert!((run.log().len() as u64) < LOG_CAP_BYTES);
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
        .split_once(FINAL_LABEL)
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

    assert!(err.contains(TIMED_OUT_MARKER), "{err}");
    assert!(!run.log_path().exists());
}

#[test]
fn a_timeout_moves_the_command_when_background_on_timeout_is_set() {
    let run = Run::start(
        json!({ "background_on_timeout": true }),
        Some(TIMEOUT_SECS),
        "echo started",
        "echo finished-late",
    );

    let receipt = run.reply().expect("the move is a success");
    assert!(receipt.contains(RECEIPT_HEAD), "{receipt}");

    run.release();
    let notice = run.final_notice();
    assert!(notice.contains("finished-late"), "{notice}");
    assert_eq!(run.log(), "started\nfinished-late\n");
}
