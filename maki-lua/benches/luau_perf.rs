//! Measures Luau VM speed under the three cancellation strategies, with the
//! Lua configured like the plugin runtime (sandbox, memory limit, O2):
//! - `mlua_hook`: mlua `set_interrupt` closure, fires at every safepoint
//!   (what the runtime used before the watchdog)
//! - `watchdog`: no resident interrupt; a thread arms a one-shot native
//!   interrupt every 10ms (what the runtime uses now)
//! - `none`: no cancellation at all (upper bound)

use std::cell::Cell;
use std::ffi::c_int;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::thread;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use mlua::{Compiler, Lua, VmState, ffi};

const MEMORY_LIMIT: usize = 512 * 1024 * 1024;
const OPT_LEVEL: u8 = 2;
const HOOK_CHECK_INTERVAL: u32 = 128;
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(10);

const FIB: &str = "\
    local function fib(n)\n\
        if n < 2 then return n end\n\
        return fib(n - 1) + fib(n - 2)\n\
    end\n\
    return fib(24)";

const BUFFER_LOOP: &str = "\
    local b = buffer.create(65536)\n\
    for i = 0, 16383 do buffer.writeu32(b, i * 4, i) end\n\
    local acc = 0\n\
    for i = 0, 16383 do acc = acc + buffer.readu32(b, i * 4) end\n\
    return acc";

#[derive(Clone, Copy)]
enum Cancellation {
    MluaHook,
    Watchdog,
    None,
}

struct WatchdogGuard {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for WatchdogGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

type InterruptFn = unsafe extern "C-unwind" fn(*mut ffi::lua_State, c_int);

/// Same atomic store the runtime's watchdog uses: the poker thread and the
/// VM thread race on this field, so plain writes would be a data race.
fn store_interrupt(state: *mut ffi::lua_State, cb: Option<InterruptFn>) {
    let raw = cb.map_or(ptr::null_mut(), |f| f as *mut ());
    unsafe {
        let slot = &raw mut (*ffi::lua_callbacks(state)).interrupt;
        AtomicPtr::from_ptr(slot.cast::<*mut ()>()).store(raw, Ordering::Release);
    }
}

unsafe extern "C-unwind" fn one_shot_interrupt(state: *mut ffi::lua_State, gc: c_int) {
    if gc >= 0 {
        return;
    }
    store_interrupt(state, None);
}

fn spawn_watchdog(lua: &Lua) -> WatchdogGuard {
    let main_state = lua.exec_raw_lua(|raw| unsafe { ffi::lua_mainthread(raw.state()) }) as usize;
    let stop = Arc::new(AtomicBool::new(false));
    let thread = thread::spawn({
        let stop = Arc::clone(&stop);
        let keep_alive = lua.clone();
        move || {
            let _keep_alive = keep_alive;
            loop {
                thread::park_timeout(WATCHDOG_POLL_INTERVAL);
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                store_interrupt(main_state as *mut ffi::lua_State, Some(one_shot_interrupt));
            }
        }
    });
    WatchdogGuard {
        stop,
        thread: Some(thread),
    }
}

fn runtime_like_lua(jit: bool, cancellation: Cancellation) -> (Lua, Option<WatchdogGuard>) {
    let lua = Lua::new();
    lua.set_compiler(Compiler::new().set_optimization_level(OPT_LEVEL));
    lua.set_memory_limit(MEMORY_LIMIT).unwrap();
    lua.sandbox(true).unwrap();
    lua.enable_jit(jit);
    let guard = match cancellation {
        Cancellation::MluaHook => {
            let shutdown = Arc::new(AtomicBool::new(false));
            let tick = Cell::new(0u32);
            lua.set_interrupt(move |_| {
                let t = tick.get().wrapping_add(1);
                tick.set(t);
                if t.is_multiple_of(HOOK_CHECK_INTERVAL) && shutdown.load(Ordering::Relaxed) {
                    return Err(mlua::Error::runtime("shutdown"));
                }
                Ok(VmState::Continue)
            });
            None
        }
        Cancellation::Watchdog => Some(spawn_watchdog(&lua)),
        Cancellation::None => None,
    };
    (lua, guard)
}

fn bench_source(c: &mut Criterion, group: &str, src: &'static str) {
    let mut g = c.benchmark_group(group);
    for (label, jit, cancellation) in [
        ("jit_mlua_hook", true, Cancellation::MluaHook),
        ("jit_watchdog", true, Cancellation::Watchdog),
        ("jit_none", true, Cancellation::None),
        ("interp_mlua_hook", false, Cancellation::MluaHook),
        ("interp_watchdog", false, Cancellation::Watchdog),
        ("interp_none", false, Cancellation::None),
    ] {
        let (lua, _guard) = runtime_like_lua(jit, cancellation);
        let f = lua.load(src).into_function().unwrap();
        g.bench_function(label, |b| b.iter(|| f.call::<i64>(()).unwrap()));
    }
    g.finish();
}

fn benches(c: &mut Criterion) {
    bench_source(c, "fib", FIB);
    bench_source(c, "buffer_rw", BUFFER_LOOP);
}

criterion_group!(luau_perf, benches);
criterion_main!(luau_perf);
