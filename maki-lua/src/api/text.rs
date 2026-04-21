use mlua::{Lua, Result as LuaResult, Table};

pub(crate) fn create_text_table(lua: &Lua) -> LuaResult<Table> {
    let text = lua.create_table()?;

    text.set(
        "html_to_markdown",
        lua.create_function(|_, html: String| {
            htmd::convert(&html).map_err(|e| mlua::Error::runtime(format!("html_to_markdown: {e}")))
        })?,
    )?;

    Ok(text)
}
