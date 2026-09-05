mod cli;
mod cmd;
mod print;
mod project_trust;
mod sdk_mode;
mod setup;
mod update;

use std::time::Duration;

use clap::Parser;

use cli::Cli;

/// How long a final telemetry export may take before maki stops waiting.
const TELEMETRY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

fn main() {
    color_eyre::install().ok();
    let result = cmd::dispatch(Cli::parse());
    // Detached export tasks die with the process; drain them here, before
    // the `exit` below skips every destructor.
    maki_otel::shutdown(TELEMETRY_SHUTDOWN_TIMEOUT);
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
