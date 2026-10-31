//! Terminal input as a stream the event loop selects on together with its
//! channels. Nothing reads the terminal while the loop is not polling, which
//! is what lets an editor or a suspended shell own the tty.

use std::future::pending;
use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, EventStream};
use futures::StreamExt;
use tracing::warn;

pub(crate) struct InputReader {
    /// `None` while paused, so no reader waits on the tty.
    stream: Option<EventStream>,
}

impl InputReader {
    /// Fails where the terminal cannot be watched, such as a piped stdin on
    /// macOS, which `EventStream::new` would panic on instead.
    pub(crate) fn new() -> io::Result<Self> {
        event::poll(Duration::ZERO)?;
        Ok(Self {
            stream: Some(EventStream::new()),
        })
    }

    /// The next event, or `None` once terminal input ends or fails. Never
    /// ready while paused.
    pub(crate) async fn next(&mut self) -> Option<Event> {
        let Some(stream) = self.stream.as_mut() else {
            return pending().await;
        };
        stream
            .next()
            .await?
            .inspect_err(|e| warn!(error = %e, "terminal input read failed"))
            .ok()
    }

    /// An event already waiting, without blocking: for coalescing a burst
    /// while one of its events is handled.
    pub(crate) fn try_next(&self) -> Option<Event> {
        matches!(event::poll(Duration::ZERO), Ok(true))
            .then(|| event::read().ok())
            .flatten()
    }

    /// Hands the tty to another process (editor, suspended shell): dropping
    /// the stream tells its reader to stop waiting on the terminal, and a
    /// fresh one takes over when the guard drops. The reader is signalled,
    /// not joined, so a key arriving in the microseconds before it stops is
    /// kept for after the resume rather than reaching the other process.
    pub(crate) fn pause(&mut self) -> PauseGuard<'_> {
        self.stream = None;
        PauseGuard(self)
    }
}

pub(crate) struct PauseGuard<'a>(&'a mut InputReader);

impl Drop for PauseGuard<'_> {
    fn drop(&mut self) {
        self.0.stream = Some(EventStream::new());
    }
}
