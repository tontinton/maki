use mlua::{Lua, Result as LuaResult, Table};

pub(crate) fn create_uv_table(lua: &Lua) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "cwd",
        lua.create_function(|_, ()| {
            Ok(std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from)))
        })?,
    )?;

    t.set(
        "os_homedir",
        lua.create_function(|_, ()| {
            Ok(dirs::home_dir().and_then(|p| p.to_str().map(String::from)))
        })?,
    )?;

    Ok(t)
}
