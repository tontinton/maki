use std::collections::HashMap;

use mlua::{FromLuaMulti, Function, IntoLuaMulti, Lua};

use crate::runtime::TaskScope;

pub(crate) const MAX_HOOK_DEPTH: u8 = 8;

#[derive(Default)]
pub(crate) struct DepthStore {
    depths: HashMap<(&'static str, String), u8>,
}

#[derive(Debug)]
pub(crate) struct DepthExceeded;

/// Reentrancy bound as RAII: `Drop` is the only decrement, so an error
/// path can never leave the depth stuck high.
pub(crate) struct DepthGuard {
    lua: Lua,
    key: (&'static str, String),
}

impl DepthGuard {
    pub(crate) fn enter(lua: &Lua, kind: &'static str, name: &str) -> Result<Self, DepthExceeded> {
        if lua.app_data_ref::<DepthStore>().is_none() {
            lua.set_app_data(DepthStore::default());
        }
        let mut store = lua
            .app_data_mut::<DepthStore>()
            .expect("DepthStore just ensured");
        let depth = store.depths.entry((kind, name.to_owned())).or_insert(0);
        if *depth >= MAX_HOOK_DEPTH {
            return Err(DepthExceeded);
        }
        *depth += 1;
        Ok(Self {
            lua: lua.clone(),
            key: (kind, name.to_owned()),
        })
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        if let Some(mut store) = self.lua.app_data_mut::<DepthStore>()
            && let Some(depth) = store.depths.get_mut(&self.key)
        {
            *depth -= 1;
            if *depth == 0 {
                store.depths.remove(&self.key);
            }
        }
    }
}

/// Calls a plugin callback so its failure stays its own: errors are logged
/// with plugin and seam name, then swallowed. The detached [`TaskScope`]
/// matters here, because a callback that inherits the firer's scope gets
/// cancelled when the firer's task dies, and plugin B should not pay for
/// plugin A's crash.
pub(crate) fn call_isolated<R: FromLuaMulti>(
    lua: &Lua,
    func: &Function,
    args: impl IntoLuaMulti,
    seam: &str,
    plugin: &str,
) -> Option<R> {
    let _scope = TaskScope::detached(lua);
    match func.call::<R>(args) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(seam, plugin, error = %e, "plugin callback failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_guard_enforces_max_and_unwinds() {
        let lua = Lua::new();
        let guards: Vec<_> = (0..MAX_HOOK_DEPTH)
            .map(|_| DepthGuard::enter(&lua, "test", "seam").expect("within bound"))
            .collect();
        assert!(DepthGuard::enter(&lua, "test", "seam").is_err());
        assert!(DepthGuard::enter(&lua, "test", "other").is_ok());
        drop(guards);
        assert!(DepthGuard::enter(&lua, "test", "seam").is_ok());
        let store = lua.app_data_ref::<DepthStore>().unwrap();
        assert!(store.depths.is_empty(), "zero entries removed");
    }

    #[test]
    fn call_isolated_swallows_errors() {
        let lua = Lua::new();
        let bad: Function = lua.load("error('boom')").into_function().unwrap();
        let ok: Function = lua.load("return 7").into_function().unwrap();
        assert!(call_isolated::<i64>(&lua, &bad, (), "seam", "p").is_none());
        assert_eq!(call_isolated::<i64>(&lua, &ok, (), "seam", "p"), Some(7));
    }
}
