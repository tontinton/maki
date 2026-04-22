use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use async_process::{Command, Stdio};
use futures_lite::StreamExt;
use futures_lite::io::{AsyncBufReadExt, BufReader};
use maki_agent::{
    AgentConfig, CancelToken, CancelTrigger, ToolDoneEvent, ToolInput, ToolOutput, ToolStartEvent,
};
use maki_providers::Message;

use super::App;

const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const SHELL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellPrefix {
    pub prefix_len: usize,
    pub command: String,
    pub visible: bool,
}

pub(crate) fn parse_shell_prefix(text: &str) -> Option<ShellPrefix> {
    let (sigil_len, visible) = if text.starts_with("!!") {
        (2, false)
    } else if text.starts_with('!') {
        (1, true)
    } else {
        return None;
    };
    let rest = &text[sigil_len..];
    let prefix_len = if rest.starts_with(' ') {
        sigil_len + 1
    } else {
        sigil_len
    };
    let command = rest.trim();
    if command.is_empty() {
        return None;
    }
    Some(ShellPrefix {
        prefix_len,
        command: command.to_owned(),
        visible,
    })
}

pub(crate) enum ShellEvent {
    Start {
        id: String,
        command: String,
    },
    Output {
        id: String,
        content: String,
    },
    Done {
        id: String,
        command: String,
        output: String,
        is_error: bool,
        visible: bool,
    },
}

#[derive(Default)]
pub(crate) struct ShellState {
    cancel_triggers: Vec<CancelTrigger>,
    pending_results: Vec<Message>,
    id_counter: u64,
}

impl ShellState {
    pub fn next_id(&mut self) -> String {
        self.id_counter += 1;
        format!("shell-{}", self.id_counter)
    }

    pub fn add_trigger(&mut self, trigger: CancelTrigger) {
        self.cancel_triggers.push(trigger);
    }

    pub fn cancel_all(&mut self) {
        for trigger in self.cancel_triggers.drain(..) {
            trigger.cancel();
        }
    }

    pub fn push_result(&mut self, msg: Message) {
        self.pending_results.push(msg);
    }

    pub fn drain_results(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.pending_results)
    }
}

impl App {
    pub(crate) fn handle_shell_event(&mut self, event: ShellEvent) {
        match event {
            ShellEvent::Start { id, command } => {
                self.main_chat().shell_tool_start(ToolStartEvent {
                    id,
                    tool: "bash".into(),
                    summary: command.clone(),
                    annotation: None,
                    input: Some(ToolInput::Code {
                        language: "bash".into(),
                        code: command,
                    }),
                    output: None,
                    render_header: None,
                });
            }
            ShellEvent::Output { id, content } => {
                self.main_chat().shell_tool_output(&id, &content);
            }
            ShellEvent::Done {
                id,
                command,
                output,
                is_error,
                visible,
            } => {
                let result_msg = if visible {
                    let label = if is_error { "Error" } else { "Output" };
                    Some(Message::user(format!(
                        "I ran: $ {command}\n\n{label}:\n{output}"
                    )))
                } else {
                    None
                };
                self.main_chat().shell_tool_done(ToolDoneEvent {
                    id,
                    tool: "bash".into(),
                    output: ToolOutput::Plain(output),
                    is_error,
                });
                if let Some(msg) = result_msg {
                    self.shell.push_result(msg);
                }
            }
        }
    }
}

pub(crate) fn spawn_shell(
    command: String,
    id: String,
    visible: bool,
    tx: flume::Sender<ShellEvent>,
    cancel: CancelToken,
    config: AgentConfig,
) {
    smol::spawn(async move {
        let _ = tx.send(ShellEvent::Start {
            id: id.clone(),
            command: command.clone(),
        });

        let result = run_command(
            &command,
            &id,
            &tx,
            &cancel,
            config.max_output_lines,
            config.max_output_bytes,
        )
        .await;

        let (output, is_error) = match result {
            Ok(out) => (out, false),
            Err(err) => (err, true),
        };

        let _ = tx.send(ShellEvent::Done {
            id,
            command,
            output,
            is_error,
            visible,
        });
    })
    .detach();
}

async fn run_command(
    command: &str,
    id: &str,
    tx: &flume::Sender<ShellEvent>,
    cancel: &CancelToken,
    max_output_lines: usize,
    max_output_bytes: usize,
) -> Result<String, String> {
    let mut std_cmd = StdCommand::new("bash");
    std_cmd
        .arg("-c")
        .arg(command)
        .env("GIT_TERMINAL_PROMPT", "0");

    #[cfg(unix)]
    unsafe {
        std_cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut cmd: Command = std_cmd.into();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("failed to spawn: {e}"))?;

    let (line_tx, line_rx) = flume::unbounded::<String>();
    if let Some(stdout) = child.stdout.take() {
        spawn_line_reader(BufReader::new(stdout), line_tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_line_reader(BufReader::new(stderr), line_tx.clone());
    }
    let mut guard = maki_agent::ChildGuard::new(child);
    drop(line_tx);

    let mut output = String::new();
    let mut line_count: usize = 0;
    let mut truncated = false;
    let mut last_flush = Instant::now();
    let deadline = Instant::now() + SHELL_TIMEOUT;

    macro_rules! race_deadline {
        ($future:expr) => {
            futures_lite::future::race(
                $future,
                futures_lite::future::race(
                    async {
                        smol::Timer::at(deadline).await;
                        Err(format!("timed out after {}s", SHELL_TIMEOUT.as_secs()))
                    },
                    async {
                        cancel.cancelled().await;
                        Err("cancelled".to_string())
                    },
                ),
            )
            .await
        };
    }

    loop {
        let line = race_deadline!(async { Ok(line_rx.recv_async().await.ok()) });
        match line {
            Ok(Some(line)) => {
                if !truncated {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&line);
                    line_count += 1;
                    if output.len() > max_output_bytes || line_count >= max_output_lines {
                        truncated = true;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                guard.kill_and_reap().await;
                return Err(e);
            }
        }

        if last_flush.elapsed() >= STREAM_FLUSH_INTERVAL && !output.is_empty() {
            flush_output(tx, id, &output);
            last_flush = Instant::now();
        }
    }

    let status =
        race_deadline!(async { guard.status().await.map_err(|e| format!("wait error: {e}")) });
    match status {
        Ok(status) => {
            flush_output(tx, id, &output);
            if truncated {
                output.push_str("\n[truncated]");
            }
            if !status.success() {
                if output.is_empty() {
                    return Err(format!("exited with code {}", status.code().unwrap_or(-1)));
                }
                return Err(output);
            }
            Ok(output)
        }
        Err(e) => {
            guard.kill_and_reap().await;
            Err(e)
        }
    }
}

fn flush_output(tx: &flume::Sender<ShellEvent>, id: &str, output: &str) {
    let _ = tx.send(ShellEvent::Output {
        id: id.to_string(),
        content: output.to_string(),
    });
}

fn spawn_line_reader<R: futures_lite::io::AsyncRead + Unpin + Send + 'static>(
    reader: BufReader<R>,
    tx: flume::Sender<String>,
) {
    smol::spawn(async move {
        let mut lines = reader.lines();
        while let Some(line) = lines.next().await {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("! ls",                     Some(ShellPrefix { prefix_len: 2, command: "ls".into(), visible: true })           ; "simple_visible")]
    #[test_case("!! ls",                    Some(ShellPrefix { prefix_len: 3, command: "ls".into(), visible: false })          ; "simple_anonymous")]
    #[test_case("! cargo test --release",   Some(ShellPrefix { prefix_len: 2, command: "cargo test --release".into(), visible: true })  ; "multi_word_command")]
    #[test_case("!! cargo build",           Some(ShellPrefix { prefix_len: 3, command: "cargo build".into(), visible: false }) ; "multi_word_anonymous")]
    #[test_case("! ",                       None                        ; "bang_space_only")]
    #[test_case("!",                        None                        ; "bang_alone")]
    #[test_case("!!",                       None                        ; "double_bang_alone")]
    #[test_case("!! ",                      None                        ; "double_bang_space_only")]
    #[test_case("hello ! world",            None                        ; "bang_mid_string")]
    #[test_case(" ! ls",                    None                        ; "leading_space")]
    #[test_case("!echo hi",                 Some(ShellPrefix { prefix_len: 1, command: "echo hi".into(), visible: true })      ; "no_space_after_bang")]
    #[test_case("!!echo hi",                Some(ShellPrefix { prefix_len: 2, command: "echo hi".into(), visible: false })     ; "no_space_after_double_bang")]
    #[test_case("!  ls",                    Some(ShellPrefix { prefix_len: 2, command: "ls".into(), visible: true })           ; "extra_spaces_trimmed")]
    fn parse_shell_prefix_cases(input: &str, expected: Option<ShellPrefix>) {
        assert_eq!(parse_shell_prefix(input), expected);
    }
}
