mod cli;
mod cmd;
mod interrupt;
mod print;
mod project_trust;
mod provider_scripts;
mod resume;
mod sdk_mode;
mod setup;
mod update;

use std::time::Duration;

use clap::Parser;

use cli::Cli;
use tokio::runtime::{Handle, Runtime};

/// How long a final telemetry export may take before maki stops waiting.
const TELEMETRY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Owns the runtime so that every way out of `main`, a panic included, shuts
/// it down without waiting for a blocking task still parked on stdin.
struct BackgroundShutdown(Option<Runtime>);

impl BackgroundShutdown {
    fn start() -> Self {
        Self(Some(
            Runtime::new().expect("failed to start the async runtime"),
        ))
    }

    fn handle(&self) -> Handle {
        self.0
            .as_ref()
            .expect("the runtime lives until drop")
            .handle()
            .clone()
    }
}

impl Drop for BackgroundShutdown {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

fn main() {
    color_eyre::install().ok();
    let runtime = BackgroundShutdown::start();
    let handle = runtime.handle();
    let guard = handle.enter();
    let result = cmd::dispatch(Cli::parse());
    // Detached export tasks die with the process; drain them here, before
    // the `exit` below skips every destructor.
    maki_otel::shutdown(TELEMETRY_SHUTDOWN_TIMEOUT);
    drop(guard);
    drop(runtime);
    if let Err(e) = result {
        print_error(&e);
        std::process::exit(1);
    }
}

fn print_error(e: &color_eyre::Report) {
    const RED: &str = "\x1b[31m";
    const BOLD_RED: &str = "\x1b[1;31m";
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";

    eprintln!();
    eprintln!("{BOLD_RED}✖ {e}{RESET}");
    let causes: Vec<_> = e.chain().skip(1).collect();
    let last = causes.len().saturating_sub(1);
    for (i, cause) in causes.iter().enumerate() {
        let branch = if i == last { "└─" } else { "├─" };
        eprintln!("{DIM}{branch}{RESET} {RED}{cause}{RESET}");
    }
    eprintln!();
}
