//! Terminal input on a dedicated thread. The event loop waits on all of its
//! channels at once (input, plugin UI actions, agent events) via a flume
//! `Selector`; blocking inside `crossterm::event::poll` would make terminal
//! input the only thing able to wake it.

use std::thread::JoinHandle;
use std::time::Duration;

use crossterm::event::{self, Event};
use tracing::warn;

/// How often the reader re-checks for control messages; bounds how long
/// pausing (before handing the tty to $EDITOR) or shutdown can take.
const CTL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const PAUSE_ACK_TIMEOUT: Duration = Duration::from_millis(500);

enum Ctl {
    Pause(flume::Sender<()>),
    Resume,
    Stop,
}

/// Reads crossterm events on its own thread and forwards them to a channel.
/// On a read error the thread exits and the channel disconnects, which the
/// event loop treats as fatal. Dropping the reader joins the thread so no
/// input is consumed after the UI hands the terminal back.
pub(crate) struct InputReader {
    rx: flume::Receiver<Event>,
    ctl_tx: flume::Sender<Ctl>,
    join: Option<JoinHandle<()>>,
}

impl InputReader {
    pub(crate) fn spawn() -> Self {
        let (tx, rx) = flume::unbounded::<Event>();
        let (ctl_tx, ctl_rx) = flume::unbounded::<Ctl>();
        let join = std::thread::Builder::new()
            .name("input-reader".into())
            .spawn(move || read_loop(&tx, &ctl_rx))
            .expect("failed to spawn input reader thread");
        Self {
            rx,
            ctl_tx,
            join: Some(join),
        }
    }

    pub(crate) fn receiver(&self) -> &flume::Receiver<Event> {
        &self.rx
    }

    /// Park the reader so another process (editor, suspended shell) can own
    /// the tty. Blocks until the reader acknowledges it is out of
    /// `event::read`; the guard resumes reading on drop.
    pub(crate) fn pause(&self) -> PauseGuard<'_> {
        let (ack_tx, ack_rx) = flume::bounded::<()>(1);
        let _ = self.ctl_tx.send(Ctl::Pause(ack_tx));
        if ack_rx.recv_timeout(PAUSE_ACK_TIMEOUT).is_err() {
            warn!("input reader did not acknowledge pause");
        }
        PauseGuard(self)
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        let _ = self.ctl_tx.send(Ctl::Stop);
        if let Some(join) = self.join.take()
            && join.join().is_err()
        {
            warn!("input reader thread panicked");
        }
    }
}

pub(crate) struct PauseGuard<'a>(&'a InputReader);

impl Drop for PauseGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.ctl_tx.send(Ctl::Resume);
    }
}

fn read_loop(tx: &flume::Sender<Event>, ctl_rx: &flume::Receiver<Ctl>) {
    loop {
        match ctl_rx.try_recv() {
            Ok(Ctl::Pause(ack)) => {
                let _ = ack.send(());
                loop {
                    match ctl_rx.recv() {
                        Ok(Ctl::Resume) => break,
                        Ok(Ctl::Pause(ack)) => {
                            let _ = ack.send(());
                        }
                        Ok(Ctl::Stop) | Err(_) => return,
                    }
                }
            }
            Ok(Ctl::Resume) | Err(flume::TryRecvError::Empty) => {}
            Ok(Ctl::Stop) | Err(flume::TryRecvError::Disconnected) => return,
        }
        match event::poll(CTL_POLL_INTERVAL) {
            Ok(false) => {}
            Ok(true) => match event::read() {
                Ok(ev) => {
                    if tx.send(ev).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "terminal input read failed");
                    return;
                }
            },
            Err(e) => {
                warn!(error = %e, "terminal input poll failed");
                return;
            }
        }
    }
}
