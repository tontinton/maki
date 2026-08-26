//! `maki.packadd`, the activation path for a package under `pack/*/opt/`.

use std::sync::{Arc, Mutex};

use maki_lua_macro::lua_fn;
use mlua::{Lua, Result as LuaResult, Table};

/// A package activation requested from Lua.
#[derive(Debug, PartialEq, Eq)]
pub enum PackOp {
    /// Load a package that is installed but not loaded.
    Activate { name: String },
}

/// What Lua recorded for the host to act on.
///
/// Session state only. Nothing here runs inline: loading an owner blocks on a
/// reply from the runtime thread that the calling Lua task is occupying, so
/// acting on a request from inside the call would wait on itself.
#[derive(Debug, Default)]
pub struct PackDeclarations {
    pub pending: Vec<PackOp>,
    /// Set once the startup drain is over.
    ///
    /// Nothing reads `pending` after that point, so a `packadd` from a command
    /// handler or a keymap would sit here for the rest of the session with no
    /// error and no log. It is refused instead.
    pub drained: bool,
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

/// Activate an installed `opt/` package, like `:packadd`.
///
/// Loading happens after this call returns. The startup package pass reports
/// an unknown, disabled, or failed package.
///
/// @param name string Package to activate.
/// @example
/// maki.packadd("maki-goal")
#[lua_fn]
fn packadd(lua: &Lua, name: String) -> LuaResult<()> {
    enqueue(lua, PackOp::Activate { name })
}

fn enqueue(lua: &Lua, op: PackOp) -> LuaResult<()> {
    let store = lua
        .app_data_ref::<PackStore>()
        .ok_or_else(|| mlua::Error::runtime("pack: not available here"))?
        .clone();
    let mut declarations = store.lock().expect("pack declarations");
    if declarations.drained {
        return Err(mlua::Error::runtime(
            "maki.packadd: packages have already been loaded, so it only works \
             while init.lua and the packages themselves are running",
        ));
    }
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
    use super::{PackOp, PackStore, add_packadd};
    use mlua::Lua;

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
    fn packadd_after_the_drain_is_refused() {
        let (lua, store) = lua_with_store();
        let maki = lua.create_table().unwrap();
        add_packadd(&lua, &maki).unwrap();
        store.lock().unwrap().drained = true;

        let f: mlua::Function = maki.get("packadd").expect("packadd is on the root table");
        let err = f
            .call::<()>("goal")
            .expect_err("nothing reads the queue after the drain");
        assert!(
            err.to_string().contains("already been loaded"),
            "got: {err}"
        );
        assert!(
            store.lock().unwrap().pending.is_empty(),
            "a refused call must not leave a request behind"
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
