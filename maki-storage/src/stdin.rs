//! The process's one reader of stdin.
//!
//! Each `tokio::io::stdin()` handle has its own buffer and its own read in
//! flight, and std's `Stdin` keeps another buffer. A reader that stops waiting,
//! like an OAuth paste the browser callback beat, takes what it had read with
//! it. The async readers of stdin go through here instead, so a cancelled read
//! leaves its bytes for the next one.

use std::io::{self, Write};
use std::sync::LazyLock;

use futures::StreamExt;
use tokio::io::{AsyncRead, Stdin, stdin};
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, LinesCodec, LinesCodecError};

static STDIN: LazyLock<Mutex<FramedRead<Stdin, LinesCodec>>> =
    LazyLock::new(|| Mutex::new(FramedRead::new(stdin(), LinesCodec::new())));

/// The next line of stdin without its line ending, or `None` once stdin is
/// closed. Cancel safe: a line read partway stays buffered for the next call.
///
/// A line that is not UTF-8 is an error, and the call after it answers `None`
/// once before reading on.
pub async fn read_line() -> io::Result<Option<String>> {
    next_line(&mut *STDIN.lock().await).await
}

/// Writes `question` to stdout and answers the next line of stdin, trimmed. A
/// closed stdin answers an empty line, as std's `read_line` does.
pub async fn prompt(question: &str) -> io::Result<String> {
    let mut stdout = io::stdout();
    write!(stdout, "{question}")?;
    stdout.flush()?;
    Ok(read_line()
        .await?
        .map(|line| line.trim().to_owned())
        .unwrap_or_default())
}

async fn next_line<R: AsyncRead + Unpin>(
    lines: &mut FramedRead<R, LinesCodec>,
) -> io::Result<Option<String>> {
    lines.next().await.transpose().map_err(|e| match e {
        LinesCodecError::Io(e) => e,
        e => io::Error::other(e),
    })
}

#[cfg(test)]
mod tests {
    use std::future::ready;

    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    const PIPE_CAPACITY: usize = 64;

    #[tokio::test]
    async fn a_dropped_read_leaves_its_bytes_for_the_next() {
        let (mut writer, pipe) = duplex(PIPE_CAPACITY);
        let mut lines = FramedRead::new(pipe, LinesCodec::new());
        writer.write_all(b"half").await.unwrap();

        tokio::select! {
            biased;
            _ = next_line(&mut lines) => panic!("the line is not complete yet"),
            () = ready(()) => {}
        }
        writer.write_all(b" a line\r\n").await.unwrap();

        assert_eq!(
            next_line(&mut lines).await.unwrap().as_deref(),
            Some("half a line")
        );
    }

    #[tokio::test]
    async fn a_line_that_is_not_utf8_is_invalid_data() {
        let mut lines = FramedRead::new(&b"\xff\n"[..], LinesCodec::new());
        let error = next_line(&mut lines).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
