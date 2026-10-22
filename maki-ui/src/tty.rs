//! Shared access to the terminal for startup capability probes: stdin may be
//! redirected, and every probe needs a deadline instead of a blocking read.

use std::fs::File;
use std::io::{IsTerminal, stdin};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};

pub(crate) fn open() -> Option<OwnedFd> {
    if stdin().is_terminal() {
        return stdin().as_fd().try_clone_to_owned().ok();
    }
    File::open("/dev/tty").ok().map(OwnedFd::from)
}

pub(crate) fn wait_readable(fd: BorrowedFd<'_>, timeout: Duration) -> bool {
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let timeout = Timespec::try_from(timeout).ok();
    poll(&mut fds, timeout.as_ref()).is_ok_and(|ready| ready > 0)
        && fds[0].revents().contains(PollFlags::IN)
}
