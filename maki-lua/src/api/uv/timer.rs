use std::sync::Arc;
use std::time::Duration;

use flume::{Receiver, Sender};
use futures::future::{self, Either};
use maki_lua_macro::{lua_class, lua_fn};
use mlua::{Function, Lua, Result as LuaResult};

use super::store::{Ctl, Status, UvEvent, ok_status, with_uv};

/// A timer, modelled after libuv's `uv_timer_t` as exposed by `vim.uv`.
/// Create with `maki.uv.new_timer()`, arm with `:start(timeout, repeat, cb)`,
/// silence with `:stop()`, release with `:close()`.
pub(crate) struct LuaTimer {
    id: u32,
}

/// Create a new timer handle. Like `vim.uv.new_timer`.
///
/// @return (maki.uv.Timer) The new handle.
/// @example
/// local timer = maki.uv.new_timer()
/// timer:start(1000, 0, function() print("one second later") end)
#[lua_fn]
fn new_timer(lua: &Lua, #[ctx] plugin: Arc<str>) -> LuaResult<LuaTimer> {
    let id = with_uv(lua, |store| store.alloc_timer(plugin));
    Ok(LuaTimer { id })
}

/// Arm the timer. Like `vim.uv.timer_start`. Fires `callback` after `timeout`
/// milliseconds, then, when `repeat` is non-zero, again every `repeat`
/// milliseconds. A `timeout` of zero fires on the next pump pass. Starting an
/// already active timer rearms it; pending ticks of the previous run are
/// dropped.
///
/// @param timeout integer Milliseconds until the first tick.
/// @param repeat integer Milliseconds between ticks; `0` for a one-shot.
/// @param callback function Called with no arguments on every tick.
/// @return (0|nil, string?, string?) `0` on success, or the fail triple.
/// @example
/// local timer = maki.uv.new_timer()
/// timer:start(500, 500, function() print("tick") end)
#[lua_fn]
fn start(
    lua: &Lua,
    this: &LuaTimer,
    timeout: u64,
    repeat: u64,
    callback: Function,
) -> LuaResult<Status> {
    let key = lua.create_registry_value(callback)?;
    let (tx, generation, ctl) =
        match with_uv(lua, |store| store.begin_timer(lua, this.id, key, repeat)) {
            Ok(triple) => triple,
            Err(err) => return Ok(err.status(lua)),
        };
    spawn_timer(
        tx,
        ctl,
        Duration::from_millis(timeout),
        Duration::from_millis(repeat),
        generation,
    );
    Ok(ok_status())
}

/// Stop the timer; the callback will not fire again. Like `vim.uv.timer_stop`.
/// Idempotent, and safe on a stopped or closed timer.
///
/// @return (0|nil, string?, string?) `0` on success, or the fail triple.
/// @example
/// timer:stop()
#[lua_fn]
fn stop(lua: &Lua, this: &LuaTimer) -> LuaResult<Status> {
    with_uv(lua, |store| store.stop_timer(lua, this.id));
    Ok(ok_status())
}

/// Close the timer and release its resources. Like `vim.uv.close`. Must be
/// called on every timer; timers of an unloaded plugin are closed for it.
///
/// @param callback function? Called with no arguments once the timer is closed.
/// @example
/// timer:close()
#[lua_fn]
fn close(lua: &Lua, this: &LuaTimer, callback: Option<Function>) -> LuaResult<()> {
    let on_close = callback
        .map(|cb| lua.create_registry_value(cb))
        .transpose()?;
    with_uv(lua, |store| store.close(lua, this.id, on_close));
    Ok(())
}

/// Whether the timer is armed. Like `vim.uv.is_active`.
///
/// @return (boolean)
/// @example
/// if timer:is_active() then timer:stop() end
#[lua_fn]
fn is_active(lua: &Lua, this: &LuaTimer) -> LuaResult<bool> {
    Ok(with_uv(lua, |store| store.is_active(this.id)))
}

/// Whether the timer is closing or closed. Like `vim.uv.is_closing`.
///
/// @return (boolean)
#[lua_fn]
fn is_closing(lua: &Lua, this: &LuaTimer) -> LuaResult<bool> {
    Ok(with_uv(lua, |store| store.is_closing(this.id)))
}

lua_class! {
    /// A timer handle, mirroring libuv's `uv_timer_t`. Created by
    /// `maki.uv.new_timer()`, armed with `:start(timeout, repeat, cb)`,
    /// silenced with `:stop()`, released with `:close()`.
    "maki.uv.Timer" => LuaTimer, TIMER_DOCS [
        start, stop, close, is_active, is_closing,
    ]
}

pub(crate) fn spawn_timer(
    tx: Sender<UvEvent>,
    ctl: Receiver<Ctl>,
    timeout: Duration,
    repeat: Duration,
    generation: u64,
) {
    smol::spawn(async move {
        let mut delay = timeout;
        loop {
            let tick = Box::pin(smol::Timer::after(delay));
            match future::select(Box::pin(ctl.recv_async()), tick).await {
                Either::Left(_) => break,
                Either::Right(_) => {}
            }
            let event = UvEvent::TimerTick { generation };
            if tx.send(event).is_err() {
                break;
            }
            if repeat.is_zero() {
                break;
            }
            delay = repeat;
        }
    })
    .detach();
}
