use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mlua::{Lua, RegistryKey, Result as LuaResult, Table, Value};

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
        id: u64,
        event: String,
        callback: RegistryKey,
        plugin: Arc<str>,
        once: bool,
    ) {
        self.listeners.entry(event).or_default().push(AutocmdEntry {
            id,
            callback,
            plugin,
            once,
        });
    }

    pub fn remove(&mut self, id: u64) -> Vec<RegistryKey> {
        let mut keys = Vec::new();
        for entries in self.listeners.values_mut() {
            if let Some(pos) = entries.iter().position(|e| e.id == id) {
                keys.push(entries.remove(pos).callback);
            }
        }
        keys
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
        lua.create_function(move |lua, (event, opts): (Value, Table)| {
            let events: Vec<String> = match event {
                Value::String(s) => vec![s.to_str()?.to_owned()],
                Value::Table(t) => t.sequence_values::<String>().collect::<LuaResult<_>>()?,
                _ => return Err(mlua::Error::runtime("event must be a string or string[]")),
            };
            let callback: mlua::Function = opts.get("callback")?;
            let once: bool = opts.get("once").unwrap_or(false);
            let id = NEXT_AUTOCMD_ID.fetch_add(1, Ordering::Relaxed);
            let mut store = lua
                .app_data_mut::<AutocmdStore>()
                .ok_or_else(|| mlua::Error::runtime("autocmd store not initialized"))?;
            for event in events {
                let key = lua.create_registry_value(callback.clone())?;
                store.register(id, event, key, Arc::clone(&p), once);
            }
            Ok(id)
        })?,
    )?;

    api_table.set(
        "del_autocmd",
        lua.create_function(|lua, id: u64| {
            let keys = lua
                .app_data_mut::<AutocmdStore>()
                .map(|mut store| store.remove(id))
                .unwrap_or_default();
            for key in keys {
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
        store.register(1, "TurnEnd".into(), key, Arc::from("test"), false);
        assert!(store.listeners["TurnEnd"].len() == 1);
        let removed = store.remove(1);
        assert_eq!(removed.len(), 1);
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

        store.register(1, "TurnEnd".into(), k1, Arc::from("plugA"), false);
        store.register(2, "TurnEnd".into(), k2, Arc::from("plugB"), false);

        let removed = store.clear_plugin("plugA");
        assert_eq!(removed.len(), 1);
        assert_eq!(store.listeners["TurnEnd"].len(), 1);
        assert_eq!(store.listeners["TurnEnd"][0].plugin.as_ref(), "plugB");
    }

    #[test]
    fn remove_nonexistent_returns_empty() {
        let mut store = AutocmdStore::default();
        assert!(store.remove(999).is_empty());
    }

    #[test]
    fn once_flag_preserved() {
        let lua = Lua::new();
        let mut store = AutocmdStore::default();
        let f = lua.create_function(|_, ()| Ok(())).unwrap();
        let key = lua.create_registry_value(f).unwrap();
        store.register(1, "TurnEnd".into(), key, Arc::from("test"), true);
        assert!(store.listeners["TurnEnd"][0].once);
    }
}
