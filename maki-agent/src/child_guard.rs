use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use async_process::Child;

const REAP_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ChildGuard {
    pid: u32,
    child: Option<Child>,
}

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        Self {
            pid: child.id(),
            child: Some(child),
        }
    }

    pub fn id(&self) -> u32 {
        self.pid
    }

    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        match self.child.as_mut() {
            Some(child) => {
                let result = child.status().await;
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
            futures_lite::future::or(
                async {
                    let _ = child.status().await;
                },
                async {
                    async_io::Timer::after(REAP_TIMEOUT).await;
                },
            )
            .await;
        }
    }

    #[cfg(unix)]
    fn signal_kill(&self) {
        if self.child.is_some() {
            unsafe {
                libc::killpg(self.pid as i32, libc::SIGKILL);
            }
        }
    }

    #[cfg(not(unix))]
    fn signal_kill(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
        }
    }

    // Best-effort non-blocking reap after SIGKILL to prevent zombie accumulation.
    // All normal paths call .status() or .kill_and_reap() first, so this only
    // fires on unexpected drops (panics, early returns). Uses WNOHANG to never
    // block the async executor.
    #[cfg(unix)]
    fn reap_nonblocking(&mut self) {
        if self.child.take().is_some() {
            unsafe {
                libc::waitpid(self.pid as i32, std::ptr::null_mut(), libc::WNOHANG);
            }
        }
    }

    #[cfg(not(unix))]
    fn reap_nonblocking(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.try_status();
        }
    }
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

    use async_process::Child;

    use super::ChildGuard;

    fn spawn_sleep() -> Child {
        let mut std_cmd = std::process::Command::new("sleep");
        std_cmd.arg("60");
        std_cmd.process_group(0);
        let mut cmd: async_process::Command = std_cmd.into();
        cmd.spawn().expect("failed to spawn sleep")
    }

    fn is_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    fn wait_for_death(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !is_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("process {pid} still alive after 2s");
    }

    #[test]
    fn drop_kills_child_process() {
        let child = spawn_sleep();
        let pid = child.id();
        assert!(is_alive(pid));
        drop(ChildGuard::new(child));
        wait_for_death(pid);
    }

    #[test]
    fn kill_and_reap_kills_process() {
        smol::block_on(async {
            let child = spawn_sleep();
            let pid = child.id();
            assert!(is_alive(pid));
            let mut guard = ChildGuard::new(child);
            guard.kill_and_reap().await;
            wait_for_death(pid);
        });
    }

    #[test]
    fn status_after_reap_returns_error() {
        smol::block_on(async {
            let child = spawn_sleep();
            let mut guard = ChildGuard::new(child);
            guard.kill_and_reap().await;
            assert!(guard.status().await.is_err());
        });
    }
}
