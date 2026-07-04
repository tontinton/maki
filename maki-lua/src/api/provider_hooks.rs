use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mlua::RegistryKey;

static NEXT_HOOK_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_hook_id() -> u64 {
    NEXT_HOOK_ID.fetch_add(1, Ordering::Relaxed)
}

pub(crate) struct ProviderHookEntry {
    pub id: u64,
    pub callback: RegistryKey,
    pub plugin: Arc<str>,
    pub slug_filter: Option<String>,
}

#[derive(Default)]
pub(crate) struct ProviderHookStore {
    pub(crate) listeners: HashMap<String, Vec<ProviderHookEntry>>,
}

impl ProviderHookStore {
    pub fn register(
        &mut self,
        id: u64,
        stage: String,
        callback: RegistryKey,
        plugin: Arc<str>,
        slug_filter: Option<String>,
    ) {
        self.listeners
            .entry(stage)
            .or_default()
            .push(ProviderHookEntry {
                id,
                callback,
                plugin,
                slug_filter,
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

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    #[test]
    fn register_and_remove() {
        let lua = Lua::new();
        let mut store = ProviderHookStore::default();
        let f = lua.create_function(|_, ()| Ok(())).unwrap();
        let key = lua.create_registry_value(f).unwrap();
        store.register(1, "request".into(), key, Arc::from("test"), None);
        assert_eq!(store.listeners["request"].len(), 1);
        let removed = store.remove(1);
        assert_eq!(removed.len(), 1);
        assert!(store.listeners["request"].is_empty());
    }

    #[test]
    fn clear_plugin_removes_only_matching() {
        let lua = Lua::new();
        let mut store = ProviderHookStore::default();

        let f1 = lua.create_function(|_, ()| Ok(())).unwrap();
        let f2 = lua.create_function(|_, ()| Ok(())).unwrap();
        let k1 = lua.create_registry_value(f1).unwrap();
        let k2 = lua.create_registry_value(f2).unwrap();

        store.register(1, "request".into(), k1, Arc::from("plugA"), None);
        store.register(2, "request".into(), k2, Arc::from("plugB"), None);

        let removed = store.clear_plugin("plugA");
        assert_eq!(removed.len(), 1);
        assert_eq!(store.listeners["request"].len(), 1);
        assert_eq!(store.listeners["request"][0].plugin.as_ref(), "plugB");
    }

    #[test]
    fn remove_nonexistent_returns_empty() {
        let mut store = ProviderHookStore::default();
        assert!(store.remove(999).is_empty());
    }

    #[test]
    fn slug_filter_preserved() {
        let lua = Lua::new();
        let mut store = ProviderHookStore::default();
        let f = lua.create_function(|_, ()| Ok(())).unwrap();
        let key = lua.create_registry_value(f).unwrap();
        store.register(
            1,
            "request".into(),
            key,
            Arc::from("test"),
            Some("my-slug".into()),
        );
        assert_eq!(
            store.listeners["request"][0].slug_filter.as_deref(),
            Some("my-slug")
        );
    }
}
