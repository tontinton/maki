use maki_lua_macro::{lua_fn, lua_table};
use maki_storage::id::MakiId;
use mlua::Lua;

/// Generate a globally unique Maki identifier.
///
/// @return string New identifier.
#[lua_fn]
fn new(_lua: &Lua) -> mlua::Result<String> {
    Ok(MakiId::generate().to_string())
}

lua_table! {
    /// Identifier utilities.
    "maki.id" => pub(crate) fn create_id_table(), DOCS [new]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_returns_unique_maki_ids() {
        let lua = Lua::new();
        let table = create_id_table(&lua).unwrap();
        let new: mlua::Function = table.get("new").unwrap();

        let first = new.call::<String>(()).unwrap();
        let second = new.call::<String>(()).unwrap();

        assert_ne!(first, second);
        assert!(first.parse::<MakiId>().is_ok());
        assert!(second.parse::<MakiId>().is_ok());
    }
}
