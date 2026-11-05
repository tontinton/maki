//! One round trip to the live terminal: write a query, read the replies.
//!
//! We tack DA1 (`ESC [ c`) onto every query. Every terminal answers it, even
//! one that ignores the rest, so its reply tells us the terminal is done
//! talking and we can stop reading long before the timeout. Call this in raw
//! mode and before the input reader starts, or the reader eats the replies.

#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
const TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(unix)]
const DA1: &[u8] = b"\x1b[c";

/// Returns what the terminal sent back, up to its DA1 reply. `None` when
/// there is no tty to ask.
#[cfg(unix)]
pub(crate) fn query(query: &[u8]) -> Option<Vec<u8>> {
    use std::fs::File;
    use std::io::{IsTerminal, Write, stdin, stdout};
    use std::os::fd::{AsFd, OwnedFd};
    use std::time::Instant;

    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::io::read;

    let tty: OwnedFd = if stdin().is_terminal() {
        stdin().as_fd().try_clone_to_owned().ok()?
    } else {
        File::open("/dev/tty").ok()?.into()
    };
    let fd = tty.as_fd();
    let mut out = stdout().lock();
    out.write_all(&[query, DA1].concat()).ok()?;
    out.flush().ok()?;
    let deadline = Instant::now() + TIMEOUT;
    let mut buf = Vec::new();
    while !da1_answered(&buf) {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let timeout = Timespec::try_from(remaining).ok();
        let readable = poll(&mut fds, timeout.as_ref()).is_ok_and(|ready| ready > 0)
            && fds[0].revents().contains(PollFlags::IN);
        if !readable {
            break;
        }
        let mut chunk = [0u8; 256];
        match read(fd, &mut chunk) {
            Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            _ => break,
        }
    }
    Some(buf)
}

#[cfg(not(unix))]
pub(crate) fn query(_query: &[u8]) -> Option<Vec<u8>> {
    None
}

/// A DA1 reply looks like `ESC [ ? 65 ; 1 ; 9 c`.
#[cfg(any(unix, test))]
fn da1_answered(buf: &[u8]) -> bool {
    find(buf, b"\x1b[?").is_some_and(|start| buf[start + 3..].contains(&b'c'))
}

pub(crate) fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(b"\x1b[?65;1;9c", true; "da1_reply")]
    #[test_case(b"\x1bP1$r0;48:2:1:2:3m\x1b\\", false; "decrqss_only")]
    #[test_case(b"\x1b[?65;1;9", false; "partial_da1")]
    fn da1(buf: &[u8], expected: bool) {
        assert_eq!(da1_answered(buf), expected);
    }
}
