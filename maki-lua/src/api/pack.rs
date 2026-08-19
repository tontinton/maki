//! `maki.packadd`, the activation path for a package under `pack/*/opt/`.

use std::sync::{Arc, Mutex};

use mlua::{Lua, Result as LuaResult, Table};

/// A change to the installed set, requested from Lua.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackOp {
    /// Load a package that is installed but not loaded.
    Activate { name: String },
}

/// What Lua recorded for the host to act on.
///
/// Session state only. Nothing here runs inline: loading an owner blocks on a
/// reply from the runtime thread that the calling Lua task is occupying, so
/// acting on a request from inside the call would wait on itself.
#[derive(Debug, Default, Clone)]
pub struct PackDeclarations {
    pub pending: Vec<PackOp>,
}

pub type PackStore = Arc<Mutex<PackDeclarations>>;

/// Registers `maki.packadd` on the root table.
///
/// It sits beside the other always-available functions rather than in a table
/// of its own, because activating code that is already installed and already
/// approved is something any plugin may do.
pub(crate) fn add_packadd(lua: &Lua, maki: &Table) -> LuaResult<()> {
    maki.set("packadd", lua.create_function(packadd)?)
}

fn packadd(lua: &Lua, name: String) -> LuaResult<()> {
    enqueue(lua, PackOp::Activate { name })
}

fn enqueue(lua: &Lua, op: PackOp) -> LuaResult<()> {
    let store = lua
        .app_data_ref::<PackStore>()
        .ok_or_else(|| mlua::Error::runtime("pack: not available here"))?
        .clone();
    let mut declarations = store.lock().expect("pack declarations");
    // Two calls naming one package still load it once. The name is checked
    // against what discovery found when the host drains this, so an unknown
    // one is reported there rather than guessed at here.
    if !declarations.pending.contains(&op) {
        declarations.pending.push(op);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with_store() -> (Lua, PackStore) {
        let lua = Lua::new();
        let store = PackStore::default();
        lua.set_app_data(store.clone());
        (lua, store)
    }

    #[test]
    fn packadd_records_each_named_package_once() {
        let (lua, store) = lua_with_store();
        let maki = lua.create_table().unwrap();
        add_packadd(&lua, &maki).unwrap();
        let f: mlua::Function = maki.get("packadd").expect("packadd is on the root table");
        f.call::<()>("goal").unwrap();
        f.call::<()>("review").unwrap();
        f.call::<()>("goal").unwrap();
        assert_eq!(
            store.lock().unwrap().pending,
            vec![
                PackOp::Activate {
                    name: "goal".to_owned()
                },
                PackOp::Activate {
                    name: "review".to_owned()
                }
            ],
            "a repeated call must not load the package twice"
        );
    }

    #[test]
    fn packadd_without_a_store_reports_rather_than_panicking() {
        let lua = Lua::new();
        let maki = lua.create_table().unwrap();
        add_packadd(&lua, &maki).unwrap();
        let f: mlua::Function = maki.get("packadd").unwrap();
        assert!(f.call::<()>("goal").is_err());
    }
}
