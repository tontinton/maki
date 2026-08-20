//! `maki.version`, modelled after Neovim's `vim.version`.

use maki_lua_macro::{lua_fn, lua_table};
use maki_pack::Version;
use mlua::{Lua, Result as LuaResult, Table};

/// Marks a table as the result of `maki.version.range`, so `pack.add` can tell
/// a constraint from an ordinary string version.
pub(crate) const RANGE_MARKER: &str = "__maki_version_range";

/// Build a semver constraint, like `vim.version.range`.
///
/// A package given a range installs the greatest tag the constraint admits.
///
/// @param spec string Constraint, for example `"^1.2"` or `">=1.0, <2.0"`.
/// @return (table) Value to pass as a spec's `version`.
/// @example
/// maki.pack.add({
///   { src = "https://github.com/user/plugin", version = maki.version.range("^1") },
/// })
#[lua_fn]
fn range(lua: &Lua, spec: String) -> LuaResult<Table> {
    // Parsed here so a bad constraint is reported where it was written, not
    // later when a package tries to install.
    Version::range(&spec)
        .map_err(|e| mlua::Error::runtime(format!("version.range: {spec:?} is not valid: {e}")))?;
    let table = lua.create_table()?;
    table.set(RANGE_MARKER, spec)?;
    Ok(table)
}

lua_table! {
    /// Version constraints, modelled after `vim.version`.
    ///
    /// ```lua
    /// local v = maki.version.range("^1.2")
    /// ```
    "maki.version" => pub(crate) fn create_version_table(), DOCS [
        range,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bad constraint is reported where it was written, not later when the
    /// package tries to install.
    #[test]
    fn an_invalid_range_fails_at_the_call_site() {
        let lua = Lua::new();
        let range_fn: mlua::Function = create_version_table(&lua).unwrap().get("range").unwrap();
        assert!(range_fn.call::<Table>("not a range").is_err());
    }

    #[test]
    fn a_valid_range_is_marked_so_pack_add_can_recognize_it() {
        let lua = Lua::new();
        let range_fn: mlua::Function = create_version_table(&lua).unwrap().get("range").unwrap();
        let t: Table = range_fn.call("^1.2").unwrap();
        assert_eq!(t.get::<String>(RANGE_MARKER).unwrap(), "^1.2");
    }
}
