use mlua::{Lua, Result as LuaResult, Table};

pub(crate) fn create_text_table(lua: &Lua) -> LuaResult<Table> {
    let text = lua.create_table()?;

    text.set(
        "html_to_markdown",
        lua.create_function(|lua, html: String| match htmd::convert(&html) {
            Ok(md) => Ok((
                mlua::Value::String(lua.create_string(&md)?),
                mlua::Value::Nil,
            )),
            Err(e) => Ok((
                mlua::Value::Nil,
                mlua::Value::String(lua.create_string(format!("html_to_markdown: {e}"))?),
            )),
        })?,
    )?;

    Ok(text)
}
