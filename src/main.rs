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

/// How long a final telemetry export may take before maki stops waiting.
const TELEMETRY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// What Rust's own panic path exits with.
const PANIC_EXIT_CODE: i32 = 101;

/// A blocking task still parked on stdin would hold up the runtime's drop,
/// so every way out of `main` exits the process instead: the normal path
/// below, and a panic through this guard.
struct ExitOnPanic;

impl Drop for ExitOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            std::process::exit(PANIC_EXIT_CODE);
        }
    }
}

#[tokio::main]
async fn main() {
    color_eyre::install().ok();
    let _exit = ExitOnPanic;
    let result = cmd::dispatch(Cli::parse()).await;
    // Detached export tasks die with the process; drain them here, before
    // the `exit` below skips every destructor.
    maki_otel::shutdown(TELEMETRY_SHUTDOWN_TIMEOUT);
    let code = match result {
        Ok(()) => 0,
        Err(e) => {
            print_error(&e);
            1
        }
    };
    std::process::exit(code)
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
