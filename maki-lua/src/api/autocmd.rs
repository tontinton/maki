use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mlua::{Lua, RegistryKey, Result as LuaResult, Table};

static NEXT_AUTOCMD_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct AutocmdEntry {
    pub id: u64,
    pub callback: RegistryKey,
    pub plugin: Arc<str>,
    pub once: bool,
}

#[derive(Default)]
pub(crate) struct AutocmdStore {
    pub(crate) listeners: HashMap<String, Vec<AutocmdEntry>>,
}

impl AutocmdStore {
    pub fn register(
        &mut self,
        event: String,
        callback: RegistryKey,
        plugin: Arc<str>,
        once: bool,
    ) -> u64 {
        let id = NEXT_AUTOCMD_ID.fetch_add(1, Ordering::Relaxed);
        self.listeners.entry(event).or_default().push(AutocmdEntry {
            id,
            callback,
            plugin,
            once,
        });
        id
    }

    pub fn remove(&mut self, id: u64) -> Option<RegistryKey> {
        for entries in self.listeners.values_mut() {
            if let Some(pos) = entries.iter().position(|e| e.id == id) {
                return Some(entries.remove(pos).callback);
            }
        }
        None
    }

    pub fn clear_plugin(&mut self, plugin: &str) -> Vec<RegistryKey> {
        let mut keys = Vec::new();
        for entries in self.listeners.values_mut() {
            let mut i = 0;
            while i < entries.len() {
                if entries[i].plugin.as_ref() == plugin {
                    keys.push(entries.remove(i).callback);
                } else {
                    i += 1;
                }
            }
        }
        self.listeners.retain(|_, v| !v.is_empty());
        keys
    }
}

pub(crate) fn add_autocmd_methods(api_table: &Table, lua: &Lua, plugin: Arc<str>) -> LuaResult<()> {
    let p = Arc::clone(&plugin);
    api_table.set(
        "create_autocmd",
        lua.create_function(move |lua, (event, opts): (String, Table)| {
            let callback: mlua::Function = opts.get("callback")?;
            let once: bool = opts.get("once").unwrap_or(false);
            let key = lua.create_registry_value(callback)?;
            let id = lua
                .app_data_mut::<AutocmdStore>()
                .ok_or_else(|| mlua::Error::runtime("autocmd store not initialized"))?
                .register(event, key, Arc::clone(&p), once);
            Ok(id)
        })?,
    )?;

    api_table.set(
        "del_autocmd",
        lua.create_function(|lua, id: u64| {
            let key = lua
                .app_data_mut::<AutocmdStore>()
                .and_then(|mut store| store.remove(id));
            if let Some(key) = key {
                let _ = lua.remove_registry_value(key);
            }
            Ok(())
        })?,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_remove() {
        let lua = Lua::new();
        let mut store = AutocmdStore::default();
        let f = lua.create_function(|_, ()| Ok(())).unwrap();
        let key = lua.create_registry_value(f).unwrap();
        let id = store.register("TurnEnd".into(), key, Arc::from("test"), false);
        assert!(store.listeners["TurnEnd"].len() == 1);
        let removed = store.remove(id);
        assert!(removed.is_some());
        assert!(store.listeners["TurnEnd"].is_empty());
    }

    #[test]
    fn clear_plugin_removes_only_matching() {
        let lua = Lua::new();
        let mut store = AutocmdStore::default();

        let f1 = lua.create_function(|_, ()| Ok(())).unwrap();
        let f2 = lua.create_function(|_, ()| Ok(())).unwrap();
        let k1 = lua.create_registry_value(f1).unwrap();
        let k2 = lua.create_registry_value(f2).unwrap();

        store.register("TurnEnd".into(), k1, Arc::from("plugA"), false);
        store.register("TurnEnd".into(), k2, Arc::from("plugB"), false);

        let removed = store.clear_plugin("plugA");
        assert_eq!(removed.len(), 1);
        assert_eq!(store.listeners["TurnEnd"].len(), 1);
        assert_eq!(store.listeners["TurnEnd"][0].plugin.as_ref(), "plugB");
    }

    #[test]
    fn remove_nonexistent_returns_none() {
        let mut store = AutocmdStore::default();
        assert!(store.remove(999).is_none());
    }

    #[test]
    fn once_flag_preserved() {
        let lua = Lua::new();
        let mut store = AutocmdStore::default();
        let f = lua.create_function(|_, ()| Ok(())).unwrap();
        let key = lua.create_registry_value(f).unwrap();
        store.register("TurnEnd".into(), key, Arc::from("test"), true);
        assert!(store.listeners["TurnEnd"][0].once);
    }
}
