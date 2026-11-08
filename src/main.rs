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

fn main() {
    let (recorder, runtime) = dial9_runtime();
    let installed = recorder.install_global_handle();
    let sampled = recorder.handle().track_current_thread();
    dial9_note(&format!(
        "global handle installed: {installed:?}, track main thread: {:?}",
        sampled.as_ref().map(|_| ())
    ));
    runtime.block_on(async_main(recorder));
}

/// Appends a line to `$DIAL9_TRACE_DIR/maki-dial9.log`, the only place a
/// message can go before the TUI owns the terminal and tracing is set up.
fn dial9_note(msg: &str) {
    use std::io::Write;
    let Some(dir) = std::env::var_os("DIAL9_TRACE_DIR") else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::path::Path::new(&dir).join("maki-dial9.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// `recorder_from_env` picks the CPU backend automatically and silently falls
/// back to per-thread timers, which only sample threads that register. This
/// forces the process-wide perf backend when it is available and says so.
fn dial9_runtime() -> dial9::AttachedRuntime {
    use dial9::cpu::{CpuProfiler, CpuProfilingConfig, SchedEventConfig};
    use dial9::memory::MemoryProfilingConfig;
    use dial9::process::ProcessResourceUsageConfig;
    use dial9::{
        Dial9HandleTokioExt, RecorderPerfExt, RecorderSourceExt, TaskDumpConfig, TokioAttachOptions,
    };

    let dir = std::env::var_os("DIAL9_TRACE_DIR")
        .filter(|_| std::env::var_os("DIAL9_ENABLED").is_some_and(|v| v == "true"));
    let Some(dir) = dir else {
        return dial9::recorder_from_env().expect("tokio runtime");
    };
    let writer = dial9::DiskBuffer::builder()
        .base_path(std::path::PathBuf::from(dir))
        .max_total_size(1 << 30)
        .rotation_period(Duration::from_secs(900))
        .build()
        .expect("dial9 trace writer");
    let builder = match CpuProfiler::start(CpuProfilingConfig::with_perf_backend()) {
        Ok(cpu) => {
            dial9_note("cpu profiler: perf (process-wide)");
            dial9::recorder(writer).source(cpu)
        }
        Err(e) => {
            dial9_note(&format!("cpu profiler: perf failed ({e}), falling back to auto"));
            dial9::recorder(writer).with_cpu_profiling(CpuProfilingConfig::default())
        }
    };
    // Every tracked thread's sched events take a perf ring out of the
    // unprivileged `perf_event_mlock_kb` budget; once it is spent, the
    // registration of a whole thread fails, CPU sampling included.
    let sched = std::env::var("DIAL9_SCHEDULE_PROFILE_ENABLED").is_none_or(|v| v != "false");
    let builder = if sched {
        builder.with_sched_events(SchedEventConfig::default())
    } else {
        builder
    };
    let recorder = builder
        .with_process_resource_usage(ProcessResourceUsageConfig::default())
        .with_memory_profiling(
            MemoryProfilingConfig::builder()
                .sample_rate_bytes(64 * 1024)
                .track_liveset(true)
                .build(),
        )
        .build();
    let mut rt = tokio::runtime::Builder::new_multi_thread();
    rt.enable_all();
    let options = TokioAttachOptions::builder()
        .runtime_name("maki")
        .task_tracking_enabled(true)
        .task_dump_config(
            TaskDumpConfig::builder()
                .idle_threshold(Duration::from_millis(5))
                .build(),
        )
        .build();
    let runtime = recorder
        .handle()
        .attach_tokio_runtime(rt, options)
        .expect("attach tokio runtime");
    recorder.enable();
    (recorder, runtime)
}

async fn async_main(recorder: dial9::Recorder) {
    color_eyre::install().ok();
    let _exit = ExitOnPanic;
    let result = cmd::dispatch(Cli::parse()).await;
    // Detached export tasks die with the process; drain them here, before
    // the `exit` below skips every destructor.
    maki_otel::shutdown(TELEMETRY_SHUTDOWN_TIMEOUT);
    recorder.graceful_shutdown(TELEMETRY_SHUTDOWN_TIMEOUT);
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
