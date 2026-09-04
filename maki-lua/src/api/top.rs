//! Top-level entries directly on the `maki` global: `defer_fn` (a
//! UI-scoped timer for "run this after N ms, not tied to my task") and
//! `notify` (a one-line notice whose default any plugin can swap via
//! `maki.set_notify_handler`).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use maki_lua_macro::{lua_class, lua_fn, lua_table};
use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table, Value};

use crate::api::util::command::{UiAction, ui_send};
use crate::runtime::{DeferQueue, DeferredCallback};

/// The one `maki.notify` override for the whole process. `create_maki_global`
/// runs per plugin load, so an override stored on the `maki` table itself would
/// only ever reach the plugin that installed it. The owner's name rides along
/// so [`clear_notify_handler`] can hand the slot back when that plugin is
/// unloaded, instead of leaving everyone calling into a dead env.
#[derive(Default)]
pub(crate) struct NotifyHandler(pub(crate) Mutex<Option<(Arc<str>, RegistryKey)>>);

/// Run {callback} after {ms} milliseconds, on the Lua thread and outside
/// any task scope. The timer does not hang off the caller's cancel token
/// or the 60 second `async.run` deadline, so the callback still fires
/// once the tool call that scheduled it is over. That is what a toast
/// needs to dismiss itself, and the difference from `maki.async.sleep`.
///
/// You get back a handle. Its `:stop()` cancels a callback that has not
/// fired yet, which is how you debounce: schedule, then stop and
/// reschedule on every new event. An error raised by the callback is
/// logged and dropped, since nobody is waiting for a result.
///
/// @param callback function Called with no arguments.
/// @param ms integer Delay in milliseconds. Zero fires on the next tick.
/// @return (maki.Timer) Handle with `:stop()` to cancel before it fires.
/// @example
/// -- A toast that dismisses itself 4 seconds later:
/// local buf = maki.ui.buf({ scratch = true })
/// buf:line("copied!")
/// local win = maki.ui.open_win(buf, { split = "right", width = 20, height = 3 })
/// maki.defer_fn(function() win:close() end, 4000)
///
/// -- Repaint only after the user has stopped typing for half a second:
/// local pending
/// local function repaint_soon()
///   if pending then
///     pending:stop()
///   end
///   pending = maki.defer_fn(repaint, 500)
/// end
#[lua_fn]
fn defer_fn(lua: &Lua, #[ctx] plugin: Arc<str>, callback: Function, ms: u64) -> LuaResult<Timer> {
    let queue = lua
        .app_data_ref::<DeferQueue>()
        .ok_or_else(|| mlua::Error::runtime("defer queue not initialized"))?;
    let func = lua.create_registry_value(callback)?;
    let cancel = Arc::new(AtomicBool::new(false));
    if let Err(rejected) = queue.push(DeferredCallback {
        func,
        delay: Duration::from_millis(ms),
        plugin,
        cancel: Arc::clone(&cancel),
    }) {
        // Reclaim the registry slot, the runtime never got the callback.
        let _ = lua.remove_registry_value(rejected.func);
        return Err(mlua::Error::runtime("defer queue closed"));
    }
    Ok(Timer { cancel })
}

/// Handle returned by `maki.defer_fn`. The runtime reads the flag once the
/// timer goes off, so flipping it before then skips the callback entirely.
pub(crate) struct Timer {
    cancel: Arc<AtomicBool>,
}

/// Cancel the pending callback. Safe to call more than once, and does
/// nothing once the callback has already run.
///
/// @return
/// @example
/// local h = maki.defer_fn(function() rebuild() end, 300)
/// h:stop()
#[lua_fn]
fn stop(_lua: &Lua, this: &Timer) -> LuaResult<()> {
    this.cancel.store(true, Ordering::Release);
    Ok(())
}

lua_class! {
    /// Handle returned by `maki.defer_fn`. Its `:stop()` cancels the
    /// callback before it fires, which is what debouncing is built on.
    "maki.Timer" => Timer, TIMER_DOCS [stop]
}

/// Show a one line notice. By default it goes to `maki.ui.flash`, with
/// `{opts.title}` in front of the message when you pass one. A run with
/// no UI, such as `maki -p` or the sdk, logs the notice instead of
/// dropping it.
///
/// There is one handler for the whole process. Once a plugin calls
/// `maki.set_notify_handler`, notices from every plugin go through it.
/// That is how a UI plugin turns flashes into stacked toasts without
/// any of the callers knowing about it.
///
/// {level} reaches the handler untouched, and the default ignores it.
///
/// @param msg string Notice text.
/// @param level string? Optional. Severity name such as "info", "warn" or "error".
/// @param opts table? Optional. `title` (string) labels the notice. Free form otherwise.
/// @return
/// @example
/// maki.notify("saved!")
/// maki.notify("build failed", "error", { title = "make" })
#[lua_fn]
fn notify(
    lua: &Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    #[ctx] plugin: Arc<str>,
    msg: String,
    level: Option<String>,
    opts: Option<Table>,
) -> LuaResult<()> {
    let title = match &opts {
        Some(t) => t.get::<Option<String>>("title")?,
        None => None,
    };
    let handler_fn = lua.app_data_ref::<NotifyHandler>().and_then(|slot| {
        let guard = locked(&slot);
        let (_, key) = guard.as_ref()?;
        lua.registry_value::<Function>(key).ok()
    });
    if let Some(func) = handler_fn {
        // A broken override must not swallow the message, so fall through.
        match func.call::<()>((msg.clone(), level.clone(), opts)) {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, "maki.notify handler failed; falling through to flash");
            }
        }
    }
    let text = match title {
        Some(title) => format!("{title}: {msg}"),
        None => msg,
    };
    if ui_send(tx.as_ref(), UiAction::Flash(text.clone())).is_err() {
        tracing::info!(plugin = %plugin, level = level.as_deref(), "{text}");
    }
    Ok(())
}

/// Install the handler that every `maki.notify` call in the process goes
/// through, in place of the default flash. Pass `nil` to put the default
/// back.
///
/// The handler runs on the Lua thread, so keep it short and hand real
/// work to `maki.async.run`. If it raises an error, the error is logged
/// and the notice falls back to `maki.ui.flash`, so the user still sees
/// it. Unloading the plugin that installed the handler also restores the
/// default.
///
/// @param handler function|nil Handler `function(msg, level?, opts?)`, or nil.
/// @return
/// @example
/// local Toast = require("maki.toast")
/// maki.set_notify_handler(function(msg, level, opts)
///   Toast.show(msg, { title = opts and opts.title, level = level })
/// end)
#[lua_fn]
fn set_notify_handler(lua: &Lua, #[ctx] plugin: Arc<str>, handler: Value) -> LuaResult<()> {
    install_notify_handler(lua, plugin, handler)
}

/// Install a `maki.notify` override into the shared slot. Reached both by
/// `set_notify_handler` and by the `maki.notify = fn` sugar that the `maki`
/// global's `__newindex` routes here.
pub(crate) fn install_notify_handler(lua: &Lua, plugin: Arc<str>, handler: Value) -> LuaResult<()> {
    // Validate the argument before touching the slot: a wrong-typed
    // handler must not silently clear a previously installed one.
    let new_key = match handler {
        Value::Nil => None,
        Value::Function(f) => Some((plugin, lua.create_registry_value(f)?)),
        _ => {
            return Err(mlua::Error::runtime(
                "set_notify_handler expects a function or nil",
            ));
        }
    };
    let slot = lua
        .app_data_ref::<NotifyHandler>()
        .ok_or_else(|| mlua::Error::runtime("notify handler slot not initialized"))?;
    let old = std::mem::replace(&mut *locked(&slot), new_key);
    if let Some((_, key)) = old {
        let _ = lua.remove_registry_value(key);
    }
    Ok(())
}

fn locked(slot: &NotifyHandler) -> std::sync::MutexGuard<'_, Option<(Arc<str>, RegistryKey)>> {
    slot.0.lock().unwrap_or_else(|e| e.into_inner())
}

/// Hand the slot back when {plugin} is unloaded. Its handler closes over an
/// env that is going away, and nobody else knows to clean up after it.
pub(crate) fn clear_notify_handler(lua: &Lua, plugin: &str) {
    let Some(slot) = lua.app_data_ref::<NotifyHandler>() else {
        return;
    };
    if let Some((_, key)) = locked(&slot).take_if(|(owner, _)| &**owner == plugin) {
        let _ = lua.remove_registry_value(key);
    }
}

lua_table! {
    extend "maki" => pub(crate) fn add_top_methods(
        plugin: Arc<str>,
    ), DOCS [
        defer_fn(plugin), manual notify, set_notify_handler(plugin),
    ]
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const BY_FUNCTION: &str = "maki.set_notify_handler(function(msg) seen = msg end)";
    const BY_ASSIGNMENT: &str = "maki.notify = function(msg) seen = msg end";

    /// Builds one plugin's `maki` table under the global {plugin}, wired the
    /// way `create_maki_global` wires it: `notify` on the metatable's
    /// `__index` so `maki.notify = fn` hits `__newindex` and lands in the
    /// shared slot instead of shadowing it.
    fn install(lua: &Lua, tx: Option<flume::Sender<UiAction>>, plugin: &str) {
        if lua.app_data_ref::<NotifyHandler>().is_none() {
            lua.set_app_data(NotifyHandler::default());
        }
        let maki = lua.create_table().unwrap();
        let owner: Arc<str> = Arc::from(plugin);
        add_top_methods(&maki, lua, Arc::clone(&owner)).unwrap();
        let index = lua.create_table().unwrap();
        notify__register(&index, lua, tx, Arc::clone(&owner)).unwrap();
        let router = lua
            .create_function(
                move |lua, (t, k, v): (Table, String, Value)| match k.as_str() {
                    "notify" => install_notify_handler(lua, Arc::clone(&owner), v),
                    _ => t.raw_set(k, v),
                },
            )
            .unwrap();
        let meta = lua.create_table().unwrap();
        meta.set("__index", index).unwrap();
        meta.set("__newindex", router).unwrap();
        maki.set_metatable(Some(meta)).unwrap();
        lua.globals().set(plugin, maki).unwrap();
    }

    fn flashed(rx: &flume::Receiver<UiAction>) -> String {
        match rx.try_recv().expect("a flash") {
            UiAction::Flash(msg) => msg,
            _ => panic!("expected Flash"),
        }
    }

    #[test_case("" ; "no handler installed")]
    #[test_case("maki.set_notify_handler(function() end) maki.set_notify_handler(nil)" ; "handler removed again")]
    fn notify_falls_back_to_flash(prelude: &str) {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded();
        install(&lua, Some(tx), "maki");
        lua.load(format!(r#"{prelude} maki.notify("hi")"#))
            .exec()
            .unwrap();
        assert_eq!(flashed(&rx), "hi");
    }

    #[test]
    fn notify_title_labels_the_flash() {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded();
        install(&lua, Some(tx), "maki");
        lua.load(r#"maki.notify("hi", nil, { title = "make" })"#)
            .exec()
            .unwrap();
        assert_eq!(flashed(&rx), "make: hi");
    }

    /// Headless runs (`maki -p`, sdk) have no UI to flash to, and a plugin
    /// should not have to care.
    #[test]
    fn notify_without_a_ui_is_not_an_error() {
        let lua = Lua::new();
        install(&lua, None, "maki");
        lua.load(r#"maki.notify("hi", "warn")"#).exec().unwrap();
    }

    /// Both spellings of "override notify" have to reach notify calls made
    /// through *any* plugin's `maki`, not just the installer's. That is the
    /// whole point of a shared slot.
    #[test_case(BY_FUNCTION ; "set_notify_handler")]
    #[test_case(BY_ASSIGNMENT ; "assignment")]
    fn override_reroutes_every_plugins_notify(install_handler: &str) {
        let lua = Lua::new();
        let (tx, _rx) = flume::unbounded();
        install(&lua, Some(tx.clone()), "maki");
        install(&lua, Some(tx), "maki_b");

        let seen: String = lua
            .load(format!(
                r#"
                seen = ""
                {install_handler}
                maki_b.notify("from the other plugin")
                return seen
            "#
            ))
            .eval()
            .unwrap();
        assert_eq!(seen, "from the other plugin");
    }

    /// The handler closes over an env that dies with its plugin, so unloading
    /// has to hand the slot back rather than leave everyone calling into it.
    #[test]
    fn unloading_the_installer_restores_the_default() {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded();
        install(&lua, Some(tx.clone()), "maki");
        install(&lua, Some(tx), "maki_b");
        lua.load(r#"seen = "" maki_b.set_notify_handler(function(msg) seen = msg end)"#)
            .exec()
            .unwrap();

        clear_notify_handler(&lua, "maki");
        lua.load(r#"maki.notify("still routed")"#).exec().unwrap();
        assert_eq!(lua.globals().get::<String>("seen").unwrap(), "still routed");
        assert!(rx.try_recv().is_err(), "the handler took it, not the flash");

        clear_notify_handler(&lua, "maki_b");
        lua.load(r#"maki.notify("back to flash")"#).exec().unwrap();
        assert_eq!(flashed(&rx), "back to flash");
    }

    #[test]
    fn notify_assignment_rejects_non_function() {
        let lua = Lua::new();
        let (tx, _rx) = flume::unbounded();
        install(&lua, Some(tx), "maki");
        let err = lua
            .load(r#"maki.notify = 42"#)
            .exec()
            .expect_err("assigning a non-function to maki.notify must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("function or nil"),
            "expected type error, got: {msg}"
        );
    }

    /// The sleep-and-dispatch half lives in the runtime, so this only pins
    /// the contract between the handle and the flag the runtime reads.
    #[test]
    fn defer_handle_stop_marks_cancel_flag() {
        let lua = Lua::new();
        lua.set_app_data(DeferQueue::new());
        install(&lua, None, "maki");
        lua.load(r#"H = maki.defer_fn(function() end, 5000)"#)
            .exec()
            .unwrap();
        let cancel = {
            let queue = lua.app_data_ref::<DeferQueue>().unwrap();
            let cb = queue.rx.try_recv().expect("callback queued");
            Arc::clone(&cb.cancel)
        };
        assert!(!cancel.load(Ordering::Acquire));
        lua.load(r#"H:stop()"#).exec().unwrap();
        assert!(cancel.load(Ordering::Acquire));
    }
}
