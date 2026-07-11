use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use mlua::{Lua, Result as LuaResult, Table, UserData, UserDataMethods};

use crate::plugin_permissions::{Permission::Env, PluginPermissions};
use crate::runtime::Request;

static NEXT_TIMER_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
const MS_PER_SEC: u64 = 1_000;

pub(crate) struct TimerEntry {
    callback: Option<mlua::RegistryKey>,
    plugin: Arc<str>,
    task: Option<smol::Task<()>>,
    cancel: Arc<AtomicBool>,
    repeat: Duration,
    started: bool,
}

impl TimerEntry {
    fn cancel_task(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.task = None;
    }
}

#[derive(Default)]
pub(crate) struct TimerStore {
    timers: HashMap<u64, TimerEntry>,
}

impl TimerStore {
    fn insert(&mut self, id: u64, entry: TimerEntry) {
        self.timers.insert(id, entry);
    }

    pub(crate) fn load_callback(&self, lua: &Lua, id: u64) -> Option<mlua::Function> {
        self.timers
            .get(&id)
            .filter(|e| e.task.is_some())
            .and_then(|e| e.callback.as_ref())
            .and_then(|key| lua.registry_value::<mlua::Function>(key).ok())
    }

    fn stop(&mut self, id: u64) {
        if let Some(entry) = self.timers.get_mut(&id) {
            entry.cancel_task();
        }
    }

    pub(crate) fn close(&mut self, lua: &Lua, id: u64) {
        if let Some(mut entry) = self.timers.remove(&id) {
            entry.cancel_task();
            if let Some(key) = entry.callback.take() {
                let _ = lua.remove_registry_value(key);
            }
        }
    }

    pub fn clear_plugin(&mut self, lua: &Lua, plugin: &str) {
        let ids: Vec<u64> = self
            .timers
            .iter()
            .filter(|(_, e)| e.plugin.as_ref() == plugin)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.close(lua, id);
        }
    }
}

struct TimerHandle {
    id: u64,
    tx: flume::Sender<Request>,
    plugin: Arc<str>,
}

impl Drop for TimerHandle {
    fn drop(&mut self) {
        let _ = self.tx.try_send(Request::TimerClose { id: self.id });
    }
}

fn spawn_timer_task(
    tx: flume::Sender<Request>,
    id: u64,
    delay: Duration,
    repeat: Duration,
    cancel: Arc<AtomicBool>,
) -> smol::Task<()> {
    smol::spawn(async move {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        smol::Timer::after(delay).await;
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if tx.send(Request::TimerFire { id }).is_err() {
            return;
        }
        if repeat.is_zero() {
            return;
        }
        loop {
            smol::Timer::after(repeat).await;
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            if tx.send(Request::TimerFire { id }).is_err() {
                return;
            }
        }
    })
}

impl UserData for TimerHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut(
            "start",
            |lua,
             this,
             (delay_ms, repeat_ms, cb): (u64, u64, mlua::Function)|
             -> mlua::Result<i64> {
                let key = lua.create_registry_value(cb)?;
                let plugin = Arc::clone(&this.plugin);
                let repeat = Duration::from_millis(repeat_ms);
                let delay = Duration::from_millis(delay_ms);
                let tx = this.tx.clone();
                let id = this.id;
                let cancel = Arc::new(AtomicBool::new(false));
                let task = spawn_timer_task(tx, id, delay, repeat, Arc::clone(&cancel));

                let Some(mut store) = lua.app_data_mut::<TimerStore>() else {
                    return Err(mlua::Error::runtime("timer store not initialized"));
                };
                let Some(entry) = store.timers.get_mut(&id) else {
                    return Err(mlua::Error::runtime("timer entry not found"));
                };
                let prev_cb = entry.callback.replace(key);
                entry.plugin = plugin;
                entry.cancel_task();
                entry.task = Some(task);
                entry.cancel = cancel;
                entry.repeat = repeat;
                entry.started = true;
                drop(store);
                if let Some(prev) = prev_cb {
                    let _ = lua.remove_registry_value(prev);
                }
                Ok(0)
            },
        );

        methods.add_method_mut("stop", |lua, this, ()| -> mlua::Result<i64> {
            if let Some(mut store) = lua.app_data_mut::<TimerStore>() {
                store.stop(this.id);
            }
            Ok(0)
        });

        methods.add_method_mut("again", |lua, this, ()| -> mlua::Result<i64> {
            let (repeat, plugin, started) = lua
                .app_data_ref::<TimerStore>()
                .and_then(|store| {
                    store
                        .timers
                        .get(&this.id)
                        .map(|e| (e.repeat, Arc::clone(&e.plugin), e.started))
                })
                .ok_or_else(|| mlua::Error::runtime("EINVAL: timer not started"))?;
            if !started {
                return Err(mlua::Error::runtime("EINVAL: timer never started"));
            }
            if repeat.is_zero() {
                if let Some(mut store) = lua.app_data_mut::<TimerStore>() {
                    store.stop(this.id);
                }
                return Ok(0);
            }
            let tx = this.tx.clone();
            let id = this.id;
            let cancel = Arc::new(AtomicBool::new(false));
            let task = spawn_timer_task(tx, id, repeat, repeat, Arc::clone(&cancel));
            if let Some(mut store) = lua.app_data_mut::<TimerStore>()
                && let Some(entry) = store.timers.get_mut(&id)
            {
                entry.plugin = plugin;
                entry.cancel_task();
                entry.task = Some(task);
                entry.cancel = cancel;
            }
            Ok(0)
        });

        methods.add_method_mut("set_repeat", |lua, this, repeat_ms: u64| {
            if let Some(mut store) = lua.app_data_mut::<TimerStore>()
                && let Some(entry) = store.timers.get_mut(&this.id)
            {
                entry.repeat = Duration::from_millis(repeat_ms);
            }
            Ok(())
        });

        methods.add_method("get_repeat", |lua, this, ()| {
            let repeat_ms = lua
                .app_data_ref::<TimerStore>()
                .and_then(|store| store.timers.get(&this.id).map(|e| e.repeat))
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            Ok(repeat_ms)
        });

        methods.add_method_mut("close", |lua, this, ()| {
            if let Some(mut store) = lua.app_data_mut::<TimerStore>() {
                store.close(lua, this.id);
            }
            Ok(())
        });

        methods.add_method("is_active", |lua, this, ()| {
            let active = lua
                .app_data_ref::<TimerStore>()
                .and_then(|store| store.timers.get(&this.id).map(|e| e.task.is_some()))
                .unwrap_or(false);
            Ok(active)
        });

        methods.add_method("is_closed", |lua, this, ()| {
            let closed = lua
                .app_data_ref::<TimerStore>()
                .map(|store| !store.timers.contains_key(&this.id))
                .unwrap_or(true);
            Ok(closed)
        });
    }
}

pub(crate) struct TimerTx(pub(crate) flume::Sender<Request>);

#[cfg(test)]
pub(crate) fn install_timer_dispatch(lua: &Lua, tx: flume::Sender<Request>) {
    lua.set_app_data(TimerStore::default());
    lua.set_app_data(TimerTx(tx));
}

pub(crate) fn create_uv_table(
    lua: &Lua,
    perms: &PluginPermissions,
    plugin: Arc<str>,
) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "cwd",
        perms.guard(Env, lua, |_, ()| {
            Ok(std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from)))
        })?,
    )?;

    t.set(
        "os_homedir",
        perms.guard(Env, lua, |_, ()| {
            Ok(maki_storage::paths::home().and_then(|p| p.to_str().map(String::from)))
        })?,
    )?;

    t.set(
        "os_getenv",
        perms.guard(Env, lua, |_, name: String| Ok(std::env::var(&name).ok()))?,
    )?;

    t.set("hrtime", lua.create_function(hrtime)?)?;

    let plugin_for_handle = Arc::clone(&plugin);
    t.set(
        "new_timer",
        lua.create_function(move |lua, ()| {
            let tx = lua
                .app_data_ref::<TimerTx>()
                .map(|t| t.0.clone())
                .ok_or_else(|| mlua::Error::runtime("timer dispatch not initialized"))?;
            let id = NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed);
            if let Some(mut store) = lua.app_data_mut::<TimerStore>() {
                store.insert(
                    id,
                    TimerEntry {
                        callback: None,
                        plugin: Arc::clone(&plugin_for_handle),
                        task: None,
                        cancel: Arc::new(AtomicBool::new(false)),
                        repeat: Duration::ZERO,
                        started: false,
                    },
                );
            }
            Ok(TimerHandle {
                id,
                tx,
                plugin: Arc::clone(&plugin_for_handle),
            })
        })?,
    )?;

    Ok(t)
}

fn hrtime(_lua: &Lua, (): ()) -> LuaResult<u64> {
    Ok(hrtime_now())
}

fn hrtime_now() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    fn store_with_entry(lua: &Lua, id: u64, plugin: &str) -> TimerStore {
        let mut store = TimerStore::default();
        let key = lua
            .create_registry_value(lua.create_function(|_, ()| Ok(())).unwrap())
            .unwrap();
        store.timers.insert(
            id,
            TimerEntry {
                callback: Some(key),
                plugin: Arc::from(plugin),
                task: None,
                cancel: Arc::new(AtomicBool::new(false)),
                repeat: Duration::from_millis(MS_PER_SEC),
                started: false,
            },
        );
        store
    }

    #[test]
    fn stop_drops_task_slot() {
        let lua = Lua::new();
        let mut store = store_with_entry(&lua, 1, "p");
        store.stop(1);
        assert!(
            store
                .timers
                .get(&1)
                .map(|e| e.task.is_none())
                .unwrap_or(false)
        );
    }

    #[test]
    fn close_removes_entry() {
        let lua = Lua::new();
        let mut store = store_with_entry(&lua, 7, "p");
        assert!(store.timers.contains_key(&7));
        store.close(&lua, 7);
        assert!(!store.timers.contains_key(&7));
    }

    #[test]
    fn close_missing_is_noop() {
        let lua = Lua::new();
        let mut store = TimerStore::default();
        store.close(&lua, 999);
        assert!(store.timers.is_empty());
    }

    #[test]
    fn clear_plugin_removes_only_matching() {
        let lua = Lua::new();
        let mut store = TimerStore::default();
        let mk = |lua: &Lua, id: u64, plug: &str| {
            let f = lua.create_function(|_, ()| Ok(())).unwrap();
            let key = lua.create_registry_value(f).unwrap();
            (
                id,
                TimerEntry {
                    callback: Some(key),
                    plugin: Arc::from(plug),
                    task: None,
                    cancel: Arc::new(AtomicBool::new(false)),
                    repeat: Duration::ZERO,
                    started: false,
                },
            )
        };
        let (i1, e1) = mk(&lua, 1, "plugA");
        let (i2, e2) = mk(&lua, 2, "plugB");
        store.timers.insert(i1, e1);
        store.timers.insert(i2, e2);
        store.clear_plugin(&lua, "plugA");
        assert!(!store.timers.contains_key(&1));
        assert!(store.timers.contains_key(&2));
        assert_eq!(store.timers[&2].plugin.as_ref(), "plugB");
    }

    #[test]
    fn cancel_flag_set_on_stop() {
        let lua = Lua::new();
        let mut store = store_with_entry(&lua, 3, "p");
        store.stop(3);
        assert!(store.timers[&3].cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn cancel_flag_set_on_close() {
        let lua = Lua::new();
        let mut store = store_with_entry(&lua, 5, "p");
        let flag = Arc::clone(&store.timers[&5].cancel);
        store.close(&lua, 5);
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn hrtime_is_monotonic_nondecreasing() {
        let a = hrtime_now();
        let b = hrtime_now();
        assert!(b >= a);
    }

    #[test]
    fn timer_handle_lifecycle_via_lua() {
        let lua = Lua::new();
        let (tx, _rx) = flume::unbounded::<Request>();
        install_timer_dispatch(&lua, tx);
        let t = create_uv_table(&lua, &PluginPermissions::trusted(), Arc::from("test")).unwrap();
        let globals = lua.globals();
        globals.set("maki", t).unwrap();
        let rep: u64 = lua
            .load(
                r#"
                local t = maki.new_timer()
                assert(not t:is_closed())
                assert(not t:is_active())
                local rc = t:start(10, 0, function() end)
                assert(rc == 0, "start returns 0 on success, got " .. tostring(rc))
                local rep = t:get_repeat()
                assert(t:is_active())
                assert(not t:is_closed())
                t:stop()
                assert(not t:is_active())
                assert(not t:is_closed())
                t:close()
                assert(not t:is_active())
                assert(t:is_closed())
                return rep
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(rep, 0);
    }

    #[test]
    fn again_on_never_started_errors() {
        let lua = Lua::new();
        let (tx, _rx) = flume::unbounded::<Request>();
        install_timer_dispatch(&lua, tx);
        let t = create_uv_table(&lua, &PluginPermissions::trusted(), Arc::from("test")).unwrap();
        let globals = lua.globals();
        globals.set("maki", t).unwrap();
        let err = lua
            .load(
                r#"
                local t = maki.new_timer()
                t:again()
                "#,
            )
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("EINVAL"), "{}", err);
    }

    #[test]
    fn new_timer_without_dispatch_errors() {
        let lua = Lua::new();
        let t = create_uv_table(&lua, &PluginPermissions::trusted(), Arc::from("test")).unwrap();
        let globals = lua.globals();
        globals.set("maki", t).unwrap();
        let result: mlua::Error = lua.load("local _ = maki.new_timer()").exec().unwrap_err();
        assert!(result.to_string().contains("timer dispatch"));
    }

    #[test]
    fn set_repeat_get_repeat_round_trip() {
        let lua = Lua::new();
        let (tx, _rx) = flume::unbounded::<Request>();
        install_timer_dispatch(&lua, tx);
        let t = create_uv_table(&lua, &PluginPermissions::trusted(), Arc::from("test")).unwrap();
        let globals = lua.globals();
        globals.set("maki", t).unwrap();
        let rep: u64 = lua
            .load(
                r#"
                local t = maki.new_timer()
                t:set_repeat(250)
                return t:get_repeat()
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(rep, 250);
    }
}
