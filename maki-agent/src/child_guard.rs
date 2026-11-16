use std::io;
use std::process::ExitStatus;
use std::time::Duration;

#[cfg(unix)]
use rustix::io::Errno;
#[cfg(unix)]
use rustix::process::{Pid, Signal, kill_process_group};
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

/// Reaping stays with tokio: dropping the `Child` queues it on tokio's orphan
/// reaper, and a `waitpid` here could reap first and leave tokio waiting on a
/// pid it no longer owns.
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.signal_kill();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::CommandExt;

    use rustix::io::Errno;
    use rustix::process::{setsid, test_kill_process};
    use tokio::process::{Child, Command};
    use tokio::task::yield_now;
    use tokio::time::timeout;

    use super::{ChildGuard, REAP_TIMEOUT, unix_pid};

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

    /// Yields between polls so the runtime gets to reap the child: `kill(pid, 0)`
    /// still succeeds on a zombie and only reports `ESRCH` once it is reaped.
    async fn wait_for_reap(pid: u32) {
        let pid = unix_pid(pid).expect("child pid is a valid pid");
        let reaped = async {
            while test_kill_process(pid) != Err(Errno::SRCH) {
                yield_now().await;
            }
        };
        timeout(REAP_TIMEOUT, reaped)
            .await
            .expect("child reaped within the timeout");
    }

    #[tokio::test]
    async fn dropped_guard_leaves_no_zombie() {
        let child = spawn_sleep();
        let pid = child.id().unwrap();
        assert!(is_alive(pid));
        drop(ChildGuard::new(child));
        wait_for_reap(pid).await;
    }

    #[tokio::test]
    async fn kill_and_reap_kills_process() {
        let child = spawn_sleep();
        let pid = child.id().unwrap();
        assert!(is_alive(pid));
        let mut guard = ChildGuard::new(child);
        guard.kill_and_reap().await;
        wait_for_reap(pid).await;
    }

    #[tokio::test]
    async fn status_after_reap_returns_error() {
        let child = spawn_sleep();
        let mut guard = ChildGuard::new(child);
        guard.kill_and_reap().await;
        assert!(guard.status().await.is_err());
    }
}
