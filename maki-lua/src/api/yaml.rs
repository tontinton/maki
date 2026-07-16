use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, LuaSerdeExt, Result as LuaResult, Value};

use super::util::convert::err_pair;

/// Turn a Lua value into a YAML string. Most Lua types work, but
/// circular references will return an error.
///
/// @param value any Lua value to encode.
/// @return (string?, string?) YAML string, or nil plus an error.
/// @example
/// local s, err = maki.yaml.encode({ name = "maki", tags = { "ai", "agent" } })
/// print(s)
#[lua_fn]
fn encode(lua: &Lua, value: Value) -> LuaResult<(Value, Value)> {
    let serde_val: serde_yaml::Value = match lua.from_value(value) {
        Ok(v) => v,
        Err(e) => return err_pair(lua, e),
    };
    match serde_yaml::to_string(&serde_val) {
        Ok(s) => Ok((Value::String(lua.create_string(&s)?), Value::Nil)),
        Err(e) => err_pair(lua, e),
    }
}

/// Parse a YAML string into a Lua value. Mappings become tables and
/// sequences become 1-indexed arrays.
///
/// @param str string YAML string to decode.
/// @return (any?, string?) Decoded value, or nil plus an error.
/// @example
/// local t, err = maki.yaml.decode("name: maki\nversion: 1")
/// print(t.name) -- maki
#[lua_fn]
fn decode(lua: &Lua, str: String) -> LuaResult<(Value, Value)> {
    match serde_yaml::from_str::<serde_yaml::Value>(&str) {
        Ok(v) => Ok((lua.to_value(&v)?, Value::Nil)),
        Err(e) => err_pair(lua, e),
    }
}

lua_table! {
    /// YAML encoding and decoding. Works the same way as `maki.json`,
    /// but for YAML formatted strings.
    ///
    /// ```lua
    /// local t = maki.yaml.decode("greeting: hello")
    /// print(t.greeting)
    /// ```
    "maki.yaml" => pub(crate) fn create_yaml_table(), DOCS [
        encode, decode,
    ]
}

#[cfg(test)]
mod tests {
    use mlua::Lua;

    fn lua_with_yaml() -> Lua {
        let lua = Lua::new();
        let yaml = super::create_yaml_table(&lua).unwrap();
        lua.globals().set("yaml", yaml).unwrap();
        lua
    }

    #[test]
    fn decode_string() {
        let lua = lua_with_yaml();
        let result: i64 = lua
            .load(r#"local t, err = yaml.decode('x: 42'); return t.x"#)
            .eval()
            .unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn decode_error_returns_nil_and_message() {
        let lua = lua_with_yaml();
        let (is_nil, has_err): (bool, bool) = lua
            .load(r#"local t, err = yaml.decode(":\n  - :\n  bad"); return t == nil, err ~= nil"#)
            .eval()
            .unwrap();
        assert!(is_nil);
        assert!(has_err);
    }

    #[test]
    fn roundtrip() {
        let lua = lua_with_yaml();
        let result: String = lua
            .load(
                r#"
                local t = {name = "test", count = 3}
                local s = yaml.encode(t)
                local t2 = yaml.decode(s)
                return t2.name .. ":" .. tostring(t2.count)
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, "test:3");
    }

    #[test]
    fn encode_error_returns_nil_and_message() {
        let lua = lua_with_yaml();
        let (is_nil, has_err): (bool, bool) = lua
            .load(
                r#"
                local bad = {}
                bad.self_ref = bad
                local s, err = yaml.encode(bad)
                return s == nil, err ~= nil
                "#,
            )
            .eval()
            .unwrap();
        assert!(is_nil);
        assert!(has_err);
    }
}
