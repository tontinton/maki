use std::process::{Command as StdCommand, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use async_process::{Child, Command};
use futures_lite::StreamExt;
use futures_lite::io::{AsyncBufReadExt, BufReader};
use maki_tool_macro::Tool;

use crate::{AgentEvent, EventSender, ToolInput, ToolOutput};

use super::{relative_path, truncate_output};
use tracing::info;

const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(100);

static RTK_AVAILABLE: LazyLock<bool> = LazyLock::new(|| {
    StdCommand::new("rtk")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
});

fn rtk_rewrite(command: &str, no_rtk: bool) -> Option<String> {
    let cmd = command.trim_start();
    if no_rtk || !*RTK_AVAILABLE ||
        // https://github.com/rtk-ai/rtk/issues/496
        (cmd.starts_with("cargo ") && cmd.contains(" -- "))
    {
        return None;
    }
    let output = StdCommand::new("rtk")
        .arg("rewrite")
        .arg(command)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let rewritten = String::from_utf8(output.stdout).ok()?;
    let trimmed = rewritten.trim();
    if trimmed.is_empty() || trimmed == command.trim() {
        return None;
    }
    Some(trimmed.to_string())
}

fn timed_out_msg(secs: u64) -> String {
    format!("command timed out after {secs}s")
}

#[derive(Tool, Debug, Clone)]
pub struct Bash {
    #[param(description = "The bash command to execute")]
    command: String,
    #[param(description = "Timeout in seconds (default 120)")]
    timeout: Option<u64>,
    #[param(description = "Working directory (default: cwd)")]
    workdir: Option<String>,
    #[param(description = "Short description (3-5 words) of what the command does")]
    description: Option<String>,
}

impl Bash {
    pub const NAME: &str = "bash";
    pub const DESCRIPTION: &str = include_str!("bash.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"command": "cargo build --release", "description": "Build release binary"},
  {"command": "git diff HEAD~1", "description": "Show last commit diff"}
]"#,
    );

    fn resolved(&self) -> (&str, Option<&str>) {
        if self.workdir.is_some() {
            return (&self.command, self.workdir.as_deref());
        }
        if let Some(rest) = self.command.strip_prefix("cd ")
            && let Some(idx) = rest.find(" && ")
        {
            let dir = rest[..idx].trim();
            if !dir.is_empty() {
                return (&rest[idx + 4..], Some(dir));
            }
        }
        (&self.command, None)
    }

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let timeout_secs = ctx
            .deadline
            .cap_timeout(self.timeout.unwrap_or(ctx.config.bash_timeout_secs))?;
        let (command, workdir) = self.resolved();
        let no_rtk = ctx.config.no_rtk;
        let cmd_owned = command.to_owned();
        let rewritten = smol::unblock(move || rtk_rewrite(&cmd_owned, no_rtk)).await;
        let command = rewritten.as_deref().unwrap_or(command);

        info!(command, workdir, timeout_secs, "bash executing");

        #[cfg(unix)]
        let mut std_cmd = {
            let mut cmd = StdCommand::new("bash");
            cmd.arg("-c").arg(command);
            cmd
        };
        #[cfg(windows)]
        let mut std_cmd = {
            let mut cmd = StdCommand::new("cmd.exe");
            cmd.arg("/C").arg(command);
            cmd
        };

        // prevent git from prompting for credentials
        std_cmd.env("GIT_TERMINAL_PROMPT", "0");

        // detach from tty so commands that try to read /dev/tty fail instead of hanging
        #[cfg(unix)]
        unsafe {
            std_cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        if let Some(dir) = workdir {
            std_cmd.current_dir(dir);
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
        let mut guard = ChildGuard::new(child);
        drop(line_tx);

        let mut output = String::new();
        let mut last_len = 0usize;
        let mut last_flush = Instant::now();

        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        enum Event {
            Line(Option<String>),
            Timeout,
            Cancel,
        }
        loop {
            match futures_lite::future::race(
                async { Event::Line(line_rx.recv_async().await.ok()) },
                futures_lite::future::race(
                    async {
                        async_io::Timer::at(deadline).await;
                        Event::Timeout
                    },
                    async {
                        ctx.cancel.cancelled().await;
                        Event::Cancel
                    },
                ),
            )
            .await
            {
                Event::Line(Some(l)) => append_line(&mut output, &l),
                Event::Line(None) => {
                    let status = guard.wait().await.map_err(|e| format!("wait error: {e}"))?;
                    flush_output(ctx, &output, &mut last_len);
                    let content = truncate_output(
                        output,
                        ctx.config.max_output_lines,
                        ctx.config.max_output_bytes,
                    );
                    if !status.success() {
                        if content.is_empty() {
                            return Err(format!(
                                "exited with code {}",
                                status.code().unwrap_or(-1)
                            ));
                        }
                        return Err(content);
                    }
                    return Ok(ToolOutput::Plain(content));
                }
                Event::Timeout => {
                    guard.kill_and_reap().await;
                    drain_remaining(&line_rx, &mut output);
                    let mut msg = timed_out_msg(timeout_secs);
                    if !output.is_empty() {
                        let content = truncate_output(
                            output,
                            ctx.config.max_output_lines,
                            ctx.config.max_output_bytes,
                        );
                        msg.push('\n');
                        msg.push_str(&content);
                    }
                    return Err(msg);
                }
                Event::Cancel => {
                    guard.kill_and_reap().await;
                    return Err("cancelled".into());
                }
            }

            if let Some(ref id) = ctx.tool_use_id
                && last_flush.elapsed() >= STREAM_FLUSH_INTERVAL
                && output.len() > last_len
            {
                send_output(&ctx.event_tx, id, &output);
                last_len = output.len();
                last_flush = Instant::now();
            }
        }
    }

    pub fn start_summary(&self) -> String {
        let (command, workdir) = self.resolved();
        let mut s = self
            .description
            .clone()
            .unwrap_or_else(|| command.to_string());
        if let Some(dir) = workdir {
            s.push_str(" in ");
            s.push_str(&relative_path(dir));
        }
        s
    }
}

impl super::ToolDefaults for Bash {
    fn start_input(&self) -> Option<ToolInput> {
        let (command, _) = self.resolved();
        Some(ToolInput::Code {
            language: "bash".into(),
            code: command.to_string(),
        })
    }

    fn start_annotation(&self) -> Option<String> {
        Some(super::timeout_annotation(self.timeout.unwrap_or(120)))
    }

    fn permission(&self) -> Option<String> {
        let (command, _) = self.resolved();
        Some(command.to_string())
    }
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.take().unwrap().status().await
    }

    async fn kill_and_reap(&mut self) {
        if let Some(mut child) = self.0.take() {
            Self::signal_kill(&mut child);
            let _ = child.status().await;
        }
    }

    fn signal_kill(child: &mut Child) {
        #[cfg(unix)]
        unsafe {
            libc::killpg(child.id() as i32, libc::SIGKILL);
        }
        #[cfg(not(unix))]
        {
            let _ = child.kill();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            Self::signal_kill(child);
        }
    }
}

fn spawn_line_reader<R: futures_lite::io::AsyncRead + Unpin + Send + 'static>(
    reader: BufReader<R>,
    tx: flume::Sender<String>,
) {
    smol::spawn(async move {
        let mut lines = reader.lines();
        while let Some(line) = lines.next().await {
            let Ok(line) = line else { break };
            if tx.try_send(line).is_err() {
                break;
            }
        }
    })
    .detach();
}

fn append_line(output: &mut String, line: &str) {
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(line);
}

fn drain_remaining(rx: &flume::Receiver<String>, output: &mut String) {
    while let Ok(line) = rx.try_recv() {
        append_line(output, &line);
    }
}

fn flush_output(ctx: &super::ToolContext, output: &str, last_len: &mut usize) {
    if let Some(ref id) = ctx.tool_use_id
        && output.len() > *last_len
    {
        send_output(&ctx.event_tx, id, output);
        *last_len = output.len();
    }
}

fn send_output(event_tx: &EventSender, id: &str, content: &str) {
    event_tx.try_send(AgentEvent::ToolOutput {
        id: id.to_string(),
        content: content.to_owned(),
    });
}

#[cfg(test)]
mod tests {
    use std::fs;
    use test_case::test_case;

    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    use super::*;

    fn bash(cmd: &str) -> Bash {
        Bash {
            command: cmd.into(),
            timeout: Some(10),
            workdir: None,
            description: None,
        }
    }

    #[test]
    fn execute_nonzero_exit_is_error() {
        smol::block_on(async {
            let ctx = stub_ctx(&AgentMode::Build);
            assert!(bash("exit 1").execute(&ctx).await.is_err());
        });
    }

    #[test]
    fn execute_timeout() {
        smol::block_on(async {
            let ctx = stub_ctx(&AgentMode::Build);
            let mut b = bash("sleep 60");
            b.timeout = Some(1);
            let err = b.execute(&ctx).await.unwrap_err();
            assert!(err.starts_with(&timed_out_msg(1)));
        });
    }

    #[test]
    fn execute_workdir() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("marker"), b"ok").unwrap();
            let ctx = stub_ctx(&AgentMode::Build);
            let mut b = bash("cat marker");
            b.workdir = Some(dir.path().to_string_lossy().into());
            let out = b.execute(&ctx).await.unwrap().as_text();
            assert_eq!(out.trim(), "ok");
        });
    }

    #[test_case(None, None, "ls",              "ls"               ; "falls_back_to_command")]
    #[test_case(Some("run tests"), None, "cargo test", "run tests"     ; "prefers_description")]
    #[test_case(Some("build"), Some("/tmp/proj"), "cargo build", "build in /tmp/proj" ; "appends_workdir")]
    #[test_case(None, None, "cd /tmp && ls", "ls in /tmp" ; "strips_cd_prefix")]
    #[test_case(Some("list"), None, "cd /tmp && ls", "list in /tmp" ; "strips_cd_prefix_with_desc")]
    fn start_summary_cases(desc: Option<&str>, workdir: Option<&str>, cmd: &str, expected: &str) {
        let b = Bash {
            command: cmd.into(),
            timeout: None,
            workdir: workdir.map(Into::into),
            description: desc.map(Into::into),
        };
        assert_eq!(b.start_summary(), expected);
    }

    #[test_case("cargo test",        None,          "cargo test"   ; "simple_command")]
    #[test_case("cd /tmp && ls",     None,          "ls"           ; "strips_cd_prefix")]
    #[test_case("cd /tmp && a && b", None,          "a && b"       ; "strips_cd_keeps_rest")]
    #[test_case("ls",                Some("/tmp"), "ls"           ; "explicit_workdir")]
    #[test_case("cd /tmp",           None,          "cd /tmp"      ; "bare_cd_unchanged")]
    fn permission_cases(cmd: &str, workdir: Option<&str>, expected: &str) {
        use crate::tools::ToolDefaults;
        let b = Bash {
            command: cmd.into(),
            timeout: None,
            workdir: workdir.map(Into::into),
            description: None,
        };
        assert_eq!(b.permission().unwrap(), expected);
    }

    #[test]
    fn tty_reading_command_fails_instead_of_hanging() {
        smol::block_on(async {
            let ctx = stub_ctx(&AgentMode::Build);
            let err = bash("head -1 /dev/tty").execute(&ctx).await.unwrap_err();
            assert!(
                !err.contains("timed out"),
                "command hung waiting for tty: {err}"
            );
        });
    }

    #[cfg(unix)]
    async fn wait_for_pidfile(path: &std::path::Path) {
        for _ in 0..200 {
            smol::Timer::after(Duration::from_millis(50)).await;
            if path.exists() {
                smol::Timer::after(Duration::from_millis(50)).await;
                return;
            }
        }
        panic!("pidfile never appeared: {}", path.display());
    }

    #[cfg(unix)]
    async fn assert_pid_dead(pidfile: &std::path::Path, msg: &str) {
        let pid: i32 = fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
        for _ in 0..60 {
            smol::Timer::after(Duration::from_millis(50)).await;
            let alive = unsafe { libc::kill(pid, 0) };
            if alive == -1 {
                return;
            }
        }
        panic!("{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn cancel_kills_process_group() {
        smol::block_on(async {
            let (trigger, cancel) = crate::cancel::CancelToken::new();
            let mut ctx = stub_ctx(&AgentMode::Build);
            ctx.cancel = cancel;

            let dir = tempfile::tempdir().unwrap();
            let pidfile = dir.path().join("pid");
            let cmd = format!("sleep 300 & echo $! > {}; wait", pidfile.display());
            let mut b = bash(&cmd);
            b.timeout = Some(10);

            let pidpath = pidfile.clone();
            smol::spawn(async move {
                wait_for_pidfile(&pidpath).await;
                trigger.cancel();
            })
            .detach();

            let err = b.execute(&ctx).await.unwrap_err();
            assert!(err.contains("cancelled"));
            assert_pid_dead(&pidfile, "grandchild process should be dead").await;
        });
    }

    #[cfg(unix)]
    #[test]
    fn future_drop_kills_process_group() {
        smol::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let pidfile = dir.path().join("pid");
            let cmd = format!("echo $$ > {}; sleep 300", pidfile.display());
            let ctx = stub_ctx(&AgentMode::Build);
            let mut b = bash(&cmd);
            b.timeout = Some(10);

            let pidpath = pidfile.clone();
            let handle = smol::spawn(async move { b.execute(&ctx).await });

            wait_for_pidfile(&pidpath).await;
            drop(handle);

            assert_pid_dead(&pidfile, "process group should be dead after future drop").await;
        });
    }
}
