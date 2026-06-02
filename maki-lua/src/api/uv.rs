use mlua::{Lua, Result as LuaResult, Table};

use crate::plugin_permissions::{Permission::Env, PluginPermissions};

pub(crate) fn create_uv_table(lua: &Lua, perms: &PluginPermissions) -> LuaResult<Table> {
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

    Ok(t)
}
