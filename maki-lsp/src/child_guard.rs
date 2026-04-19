use async_process::Child;

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
