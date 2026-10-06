//! Background jobs: run one on a backend and get its output as it runs.
//!
//! [`FsBackend::exec`](crate::FsBackend::exec) is the one shape for running a
//! command: it streams every output line to a [`JobSink`] and returns the exit
//! code, so a caller sees the run as it happens rather than after it ended.
//! [`FsBackend::exec_job`](crate::FsBackend::exec_job) is the same run in the
//! background: the sink also gets the exit, and the caller gets a handle to
//! stop the process with.

use std::collections::HashMap;
use std::path::PathBuf;
#[cfg(windows)]
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use shell_words::join as shell_join;

/// A command to run: a shell line, or an argv the caller built itself so
/// there are no quoting rules to get wrong.
pub enum JobCommand {
    Shell(String),
    Argv(Vec<String>),
}

impl From<&str> for JobCommand {
    fn from(cmd: &str) -> Self {
        Self::Shell(cmd.to_string())
    }
}

impl JobCommand {
    /// The command as one shell line: the line itself, or the argv joined and
    /// quoted. What a backend that can only run a line gets, and what a job
    /// row shows.
    pub fn display(&self) -> String {
        match self {
            Self::Shell(cmd) => cmd.clone(),
            Self::Argv(argv) => shell_join(argv),
        }
    }
}

/// Which of a job's streams a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStream {
    Stdout,
    Stderr,
}

/// Where a running job reports its output and, last, its exit.
pub trait JobSink: Send + Sync {
    fn line(&self, stream: JobStream, line: String);
    fn exit(&self, code: i32);
}

/// Splits a raw output stream into the lines a [`JobSink`] hears, holding the
/// trailing partial line until its newline arrives. Every backend reads its
/// streams through this, so one command's lines read the same whether it ran
/// on the host or inside the sandbox, and a multi-byte character split across
/// two reads still decodes as one.
#[derive(Default)]
pub struct LineSplitter {
    partial: Vec<u8>,
}

impl LineSplitter {
    /// Add what one read returned, taking out every line it completed.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for chunk in bytes.split_inclusive(|b| *b == b'\n') {
            let (line, complete) = match chunk.split_last() {
                Some((b'\n', rest)) => (rest, true),
                _ => (chunk, false),
            };
            let line = match line.split_last() {
                // A CR only ends the line when its LF did.
                Some((b'\r', rest)) if complete => rest,
                _ => line,
            };
            if !complete {
                self.partial.extend_from_slice(line);
                continue;
            }
            let line = if self.partial.is_empty() {
                String::from_utf8_lossy(line).into_owned()
            } else {
                self.partial.extend_from_slice(line);
                String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned()
            };
            lines.push(line);
        }
        lines
    }

    /// Take out the trailing line a stream ended without a newline on, if it
    /// wrote one at all.
    pub fn flush(&mut self) -> Option<String> {
        (!self.partial.is_empty())
            .then(|| String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned())
    }
}

/// Where one of a job's streams goes.
pub enum JobOut {
    /// Piped, so the sink sees its lines as they arrive.
    Capture,
    /// Dropped: no line of this stream ever reaches the sink.
    Discard,
    /// The child appends to the file on its own: no line, no reader.
    File(PathBuf),
}

/// A job to run in the background.
pub struct JobRequest {
    pub command: JobCommand,
    pub workdir: Option<PathBuf>,
    pub env: Option<HashMap<String, String>>,
    pub stdout: JobOut,
    pub stderr: JobOut,
    pub sink: Arc<dyn JobSink>,
}

/// The process a running job owns, and whether it is still ours to signal.
pub struct JobHandle {
    pid: u32,
    /// Set by the backend the moment the child is reaped, which is well
    /// before the exit is reported. Signalling a reaped pid hits whoever the
    /// kernel handed it to next; until then the child is a zombie, and a
    /// zombie group leader keeps its pid and pgid off the free list, so the
    /// group is still the right target. The flag carries no data of its own,
    /// hence `Relaxed`.
    reaped: Arc<AtomicBool>,
}

impl JobHandle {
    /// Take ownership of a process the backend just spawned. {reaped} is the
    /// flag its wait thread must store into when it reaps.
    pub fn spawned(pid: u32, reaped: Arc<AtomicBool>) -> Self {
        Self { pid, reaped }
    }

    /// A job with no process of its own to signal.
    pub fn none() -> Self {
        Self::spawned(0, Arc::new(AtomicBool::new(false)))
    }

    /// Pid of the job's process, or 0 when it has none.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the process is gone, so its pid may already belong to someone
    /// else.
    pub fn is_reaped(&self) -> bool {
        self.reaped.load(Ordering::Relaxed)
    }

    /// Kill the job and everything it started, unless it is already gone.
    pub fn kill(&self) {
        if self.pid == 0 || self.is_reaped() {
            return;
        }
        #[cfg(unix)]
        {
            use rustix::process::{Pid, Signal, kill_process_group};
            if let Ok(raw) = i32::try_from(self.pid)
                && let Some(pid) = Pid::from_raw(raw)
            {
                let _ = kill_process_group(pid, Signal::KILL);
            }
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &self.pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use serde_json::Value;
    use test_case::test_case;

    use super::*;
    use crate::grep::{GrepFileEntry, GrepParams};
    use crate::{FsBackend, FsError};

    const NEVER_REAPED: &str = "a job that ran to completion must be reaped before its exit";
    const NEVER_EXITED: &str = "job never reported its exit";
    const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
    const POLL_INTERVAL: Duration = Duration::from_millis(10);

    /// A backend whose `exec` answers with a canned reply instead of running
    /// anything. Its fs side is never reached by a job.
    struct Fake(Result<(Vec<String>, i32), String>);

    impl FsBackend for Fake {
        fn read(&self, _: &Path, _: u64) -> Result<Vec<u8>, FsError> {
            unreachable!()
        }
        fn metadata(&self, _: &Path) -> Result<Value, FsError> {
            unreachable!()
        }
        fn dir(&self, _: &Path, _: u32) -> Result<Value, FsError> {
            unreachable!()
        }
        fn exists(&self, _: &Path) -> Result<bool, FsError> {
            unreachable!()
        }
        fn write(&self, _: &Path, _: &[u8]) -> Result<(), FsError> {
            unreachable!()
        }
        fn append(&self, _: &Path, _: &[u8]) -> Result<(), FsError> {
            unreachable!()
        }
        fn atomic_write(&self, _: &Path, _: &[u8]) -> Result<(), FsError> {
            unreachable!()
        }
        fn remove(&self, _: &Path, _: bool) -> Result<(), FsError> {
            unreachable!()
        }
        fn mkdir(&self, _: &Path, _: bool) -> Result<(), FsError> {
            unreachable!()
        }
        fn glob(
            &self,
            _: &Path,
            _: &[String],
            _: bool,
            _: bool,
            _: Option<usize>,
        ) -> Result<Vec<PathBuf>, FsError> {
            unreachable!()
        }
        fn grep(&self, _: GrepParams) -> Result<Vec<GrepFileEntry>, FsError> {
            unreachable!()
        }
        fn exec(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<u64>,
            sink: Arc<dyn JobSink>,
        ) -> Result<i32, FsError> {
            let (lines, code) = self.0.clone().map_err(FsError::new)?;
            for line in lines {
                sink.line(JobStream::Stdout, line);
            }
            Ok(code)
        }
    }

    #[derive(Default)]
    struct Recorded {
        lines: Vec<(JobStream, String)>,
        exit: Option<i32>,
    }

    struct Recorder(Arc<Mutex<Recorded>>);

    impl JobSink for Recorder {
        fn line(&self, stream: JobStream, line: String) {
            self.0.lock().unwrap().lines.push((stream, line));
        }
        fn exit(&self, code: i32) {
            self.0.lock().unwrap().exit = Some(code);
        }
    }

    /// Run a whole blocking job to completion, as a caller of the default
    /// `exec_job` would experience it.
    fn run_blocking(
        reply: Result<(Vec<String>, i32), String>,
    ) -> (Arc<Mutex<Recorded>>, JobHandle) {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let handle = Arc::new(Fake(reply))
            .exec_job(JobRequest {
                command: JobCommand::from("echo hi"),
                workdir: None,
                env: None,
                stdout: JobOut::Capture,
                stderr: JobOut::Capture,
                sink: Arc::new(Recorder(Arc::clone(&recorded))),
            })
            .expect("job started");
        let deadline = Instant::now() + EXIT_TIMEOUT;
        while recorded.lock().unwrap().exit.is_none() {
            assert!(Instant::now() < deadline, "{NEVER_EXITED}");
            sleep(POLL_INTERVAL);
        }
        (recorded, handle)
    }

    #[test]
    fn a_default_job_reports_its_lines_then_the_exit() {
        let lines = ["one".to_string(), "two".to_string()].to_vec();
        let (recorded, handle) = run_blocking(Ok((lines, 3)));
        let recorded = recorded.lock().unwrap();

        assert_eq!(
            recorded.lines,
            [
                (JobStream::Stdout, "one".to_string()),
                (JobStream::Stdout, "two".to_string())
            ]
        );
        assert_eq!(recorded.exit, Some(3));
        assert!(handle.is_reaped(), "{NEVER_REAPED}");
    }

    #[test]
    fn a_command_that_cannot_run_reports_the_error_and_exits_one() {
        const NO_CHILD: &str = "no child";
        let (recorded, _) = run_blocking(Err(NO_CHILD.into()));
        let recorded = recorded.lock().unwrap();

        assert!(
            recorded
                .lines
                .iter()
                .any(|(_, line)| line.contains(NO_CHILD)),
            "the reason the job never ran has to reach the sink: {:?}",
            recorded.lines
        );
        assert_eq!(recorded.exit, Some(1));
    }

    /// A line only reaches the sink once its newline does, so a command that
    /// writes without one keeps its last line until the stream ends.
    #[test_case(b"one\ntwo\nthree", &["one", "two"], Some("three"); "no_trailing_newline")]
    #[test_case(b"one\ntwo\n", &["one", "two"], None; "every_line_complete")]
    #[test_case(b"\n", &[""], None; "empty_line")]
    #[test_case(b"one\r\n", &["one"], None; "crlf")]
    #[test_case(b"one\rtwo", &[], Some("one\rtwo"); "bare_cr_is_not_a_line_break")]
    #[test_case(b"", &[], None; "flush_of_an_empty_stream")]
    #[test_case(b"tail", &[], Some("tail"); "flush_returns_the_remainder")]
    #[test_case(b"one\ntwo\nthree\n", &["one", "two", "three"], None; "one_read_many_lines")]
    fn lines_split_on_newlines_only(bytes: &[u8], before_flush: &[&str], tail: Option<&str>) {
        let mut splitter = LineSplitter::default();
        assert_eq!(splitter.push(bytes), before_flush);
        assert_eq!(splitter.flush().as_deref(), tail);
    }

    /// A read can end mid-character: the halves only decode as one string once
    /// they are joined.
    #[test]
    fn a_line_split_across_reads_joins_back_into_one() {
        let mut line = vec![0xC3, 0xA9];
        line.extend_from_slice(b"fin\n");
        let (head, tail) = line.split_at(1);
        let mut splitter = LineSplitter::default();

        assert!(splitter.push(head).is_empty());
        assert_eq!(splitter.push(tail), ["éfin"]);
    }

    #[test]
    fn an_argv_row_reads_back_as_the_same_argv() {
        const ARGV: [&str; 2] = ["echo", "a; echo pwned"];
        let row = JobCommand::Argv(ARGV.map(String::from).to_vec()).display();
        assert_eq!(
            shell_words::split(&row).unwrap(),
            ARGV,
            "the row a user reads must quote what the shell would have eaten"
        );
    }

    /// A backend that ran a command without a process of its own hands back a
    /// handle with pid 0, which is this process's own group. `kill` has to
    /// stop there, or `jobstop` on such a job would take the caller with it.
    #[test]
    fn a_job_without_a_process_is_never_signalled() {
        JobHandle::none().kill();
    }
}
