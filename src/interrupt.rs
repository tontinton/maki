//! SIGINT and SIGTERM in the modes without a terminal UI. By default a signal
//! kills maki on the spot: the processes its tools started keep running as
//! orphans, and no result is printed. Here the first signal stops the
//! session instead, which stops that work, and maki exits as the signal would
//! have once it is torn down. A second signal exits at once, in case the stop
//! wedges. A signal the parent started maki with ignored stays ignored, as
//! with `maki -p ... &` in a script.

use std::process;
use std::sync::{Arc, OnceLock};
use std::thread;

use async_signal::{Signal, Signals};
use maki_agent::{CancelToken, CancelTrigger};
use tracing::{info, warn};

#[cfg(unix)]
const WATCHED: [Signal; 2] = [Signal::Int, Signal::Term];
#[cfg(not(unix))]
const WATCHED: [Signal; 1] = [Signal::Int];
/// A shell reports a process a signal killed as 128 plus the signal number.
const SIGNAL_EXIT_BASE: i32 = 128;
const FORCED_EXIT: &str = "maki: signalled again, exiting without stopping background work";

pub struct Interrupt {
    stop: CancelToken,
    caught: Arc<OnceLock<Signal>>,
}

impl Interrupt {
    pub fn install() -> Self {
        let (trigger, stop) = CancelToken::new();
        let caught = Arc::new(OnceLock::new());
        match Signals::new(
            WATCHED
                .into_iter()
                .filter(|&signal| !inherited_ignore(signal)),
        ) {
            Ok(signals) => {
                let caught = Arc::clone(&caught);
                thread::spawn(move || watch(signals, trigger, &caught));
            }
            Err(error) => warn!(%error, "cannot catch SIGINT/SIGTERM, a signal kills maki at once"),
        }
        Self { stop, caught }
    }

    /// Fires on the first signal.
    pub fn stop(&self) -> CancelToken {
        self.stop.clone()
    }

    /// Call once everything is torn down: exits as the caught signal would
    /// have, and returns when none was caught.
    pub fn exit_if_caught(&self) {
        if let Some(&signal) = self.caught.get() {
            maki_otel::shutdown(crate::TELEMETRY_SHUTDOWN_TIMEOUT);
            process::exit(exit_code(signal));
        }
    }
}

fn watch(signals: Signals, trigger: CancelTrigger, caught: &OnceLock<Signal>) {
    let mut signals = smol::stream::block_on(signals).filter_map(Result::ok);
    let Some(first) = signals.next() else {
        return;
    };
    info!(signal = ?first, "signal caught, stopping the session");
    let _ = caught.set(first);
    trigger.cancel();
    if let Some(second) = signals.next() {
        eprintln!("{FORCED_EXIT}");
        process::exit(exit_code(second));
    }
}

/// Non-interactive shells start background jobs with SIGINT ignored, and
/// installing a handler would replace that.
#[cfg(unix)]
fn inherited_ignore(signal: Signal) -> bool {
    // SAFETY: a null `act` only reads the current action into `current`.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signal as i32, std::ptr::null(), &mut current) == 0
            && current.sa_sigaction == libc::SIG_IGN
    }
}

#[cfg(not(unix))]
fn inherited_ignore(_: Signal) -> bool {
    false
}

fn exit_code(signal: Signal) -> i32 {
    SIGNAL_EXIT_BASE + signal as i32
}

#[cfg(all(test, unix))]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case(Signal::Int, 130 ; "sigint")]
    #[test_case(Signal::Term, 143 ; "sigterm")]
    fn exits_as_a_shell_reports_the_signal(signal: Signal, expected: i32) {
        assert_eq!(exit_code(signal), expected);
    }

    /// One test, so no other thread flips the disposition between a set and
    /// its read.
    #[test]
    fn reads_whether_the_parent_ignored_the_signal() {
        for (action, ignored) in [(libc::SIG_IGN, true), (libc::SIG_DFL, false)] {
            // SAFETY: SIGUSR1 is not watched or handled anywhere else in maki.
            unsafe { libc::signal(Signal::Usr1 as i32, action) };

            assert_eq!(inherited_ignore(Signal::Usr1), ignored);
        }
    }
}
