use std::io;
use std::process::ExitStatus;
use std::time::Duration;

#[cfg(unix)]
use rustix::io::Errno;
#[cfg(unix)]
use rustix::process::{Pid, Signal, WaitOptions, kill_process_group, waitpid};
use tokio::process::Child;
use tokio::time::timeout;

const REAP_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ChildGuard {
    pid: u32,
    child: Option<Child>,
}

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        Self {
            pid: child
                .id()
                .expect("a child that was not waited on has a pid"),
            child: Some(child),
        }
    }

    pub fn id(&self) -> u32 {
        self.pid
    }

    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        match self.child.as_mut() {
            Some(child) => {
                let result = child.wait().await;
                if result.is_ok() {
                    self.child = None;
                }
                result
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child already reaped",
            )),
        }
    }

    pub async fn kill_and_reap(&mut self) {
        self.signal_kill();
        if let Some(mut child) = self.child.take() {
            let _ = timeout(REAP_TIMEOUT, child.wait()).await;
        }
    }

    #[cfg(unix)]
    fn signal_kill(&self) {
        if self.child.is_some() {
            sigkill_process_group(self.pid);
        }
    }

    #[cfg(not(unix))]
    fn signal_kill(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }

    // Best-effort non-blocking reap after SIGKILL to prevent zombie accumulation.
    // All normal paths call .status() or .kill_and_reap() first, so this only
    // fires on unexpected drops (panics, early returns). Uses WNOHANG to never
    // block the async executor.
    #[cfg(unix)]
    fn reap_nonblocking(&mut self) {
        if self.child.take().is_some()
            && let Some(pid) = unix_pid(self.pid)
        {
            let _ = waitpid(Some(pid), WaitOptions::NOHANG);
        }
    }

    #[cfg(not(unix))]
    fn reap_nonblocking(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.try_wait();
        }
    }
}

#[cfg(unix)]
pub(crate) fn sigkill_process_group(pid: u32) {
    let Some(group) = unix_pid(pid) else {
        return;
    };
    if let Err(err) = kill_process_group(group, Signal::KILL)
        && err != Errno::SRCH
    {
        tracing::warn!(pid, %err, "failed to kill process group");
    }
}

#[cfg(unix)]
fn unix_pid(pid: u32) -> Option<Pid> {
    i32::try_from(pid).ok().and_then(Pid::from_raw)
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.signal_kill();
        }
        self.reap_nonblocking();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    use rustix::process::{setsid, test_kill_process};
    use tokio::process::{Child, Command};
    use tokio::time::sleep;

    use super::{ChildGuard, unix_pid};

    fn spawn_sleep() -> Child {
        let mut std_cmd = std::process::Command::new("sleep");
        std_cmd.arg("60");
        unsafe {
            std_cmd.pre_exec(|| {
                let _ = setsid();
                Ok(())
            });
        }
        Command::from(std_cmd)
            .spawn()
            .expect("failed to spawn sleep")
    }

    fn is_alive(pid: u32) -> bool {
        unix_pid(pid).is_some_and(|pid| test_kill_process(pid).is_ok())
    }

    /// Awaits between polls so the runtime reaps the child once it is dead:
    /// `kill(pid, 0)` still succeeds on a zombie.
    async fn wait_for_death(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !is_alive(pid) {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("process {pid} still alive after 2s");
    }

    #[tokio::test]
    async fn drop_kills_child_process() {
        let child = spawn_sleep();
        let pid = child.id().unwrap();
        assert!(is_alive(pid));
        drop(ChildGuard::new(child));
        wait_for_death(pid).await;
    }

    #[tokio::test]
    async fn kill_and_reap_kills_process() {
        let child = spawn_sleep();
        let pid = child.id().unwrap();
        assert!(is_alive(pid));
        let mut guard = ChildGuard::new(child);
        guard.kill_and_reap().await;
        wait_for_death(pid).await;
    }

    #[tokio::test]
    async fn status_after_reap_returns_error() {
        let child = spawn_sleep();
        let mut guard = ChildGuard::new(child);
        guard.kill_and_reap().await;
        assert!(guard.status().await.is_err());
    }
}
