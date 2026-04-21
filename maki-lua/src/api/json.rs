use mlua::{Lua, LuaSerdeExt, Result as LuaResult, Table, Value};

fn err_pair(lua: &Lua, e: impl std::fmt::Display) -> LuaResult<(Value, Value)> {
    Ok((Value::Nil, Value::String(lua.create_string(e.to_string())?)))
}

pub(crate) fn create_json_table(lua: &Lua) -> LuaResult<Table> {
    let json = lua.create_table()?;

    json.set(
        "encode",
        lua.create_function(|lua, value: Value| {
            let serde_val: serde_json::Value = match lua.from_value(value) {
                Ok(v) => v,
                Err(e) => return err_pair(lua, e),
            };
            match serde_json::to_string(&serde_val) {
                Ok(s) => Ok((Value::String(lua.create_string(&s)?), Value::Nil)),
                Err(e) => err_pair(lua, e),
            }
        })?,
    )?;

    json.set(
        "decode",
        lua.create_function(|lua, s: String| {
            match serde_json::from_str::<serde_json::Value>(&s) {
                Ok(v) => Ok((lua.to_value(&v)?, Value::Nil)),
                Err(e) => err_pair(lua, e),
            }
        })?,
    )?;

    Ok(json)
}

#[cfg(test)]
mod tests {
    use mlua::Lua;

    fn lua_with_json() -> Lua {
        let lua = Lua::new();
        let json = super::create_json_table(&lua).unwrap();
        lua.globals().set("json", json).unwrap();
        lua
    }

    #[test]
    fn encode_table() {
        let lua = lua_with_json();
        let result: String = lua
            .load(r#"local s, err = json.encode({a = 1}); return s"#)
            .eval()
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["a"], 1);
    }

    #[test]
    fn decode_string() {
        let lua = lua_with_json();
        let result: i64 = lua
            .load(r#"local t, err = json.decode('{"x":42}'); return t.x"#)
            .eval()
            .unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn encode_error_returns_nil_and_message() {
        let lua = lua_with_json();
        let (is_nil, has_err): (bool, bool) = lua
            .load(r#"local s, err = json.encode(json.encode); return s == nil, err ~= nil"#)
            .eval()
            .unwrap();
        assert!(is_nil);
        assert!(has_err);
    }

    #[test]
    fn decode_error_returns_nil_and_message() {
        let lua = lua_with_json();
        let (is_nil, has_err): (bool, bool) = lua
            .load(r#"local t, err = json.decode("{invalid}"); return t == nil, err ~= nil"#)
            .eval()
            .unwrap();
        assert!(is_nil);
        assert!(has_err);
    }

    #[test]
    fn roundtrip() {
        let lua = lua_with_json();
        let result: String = lua
            .load(
                r#"
                local t = {name = "test", count = 3}
                local s = json.encode(t)
                local t2 = json.decode(s)
                return t2.name .. ":" .. tostring(t2.count)
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, "test:3");
    }

    #[test]
    fn decode_array() {
        let lua = lua_with_json();
        let result: i64 = lua
            .load(r#"local t = json.decode('[10,20,30]'); return #t"#)
            .eval()
            .unwrap();
        assert_eq!(result, 3);
    }

    #[test]
    fn decode_null_roundtrips() {
        let lua = lua_with_json();
        let result: String = lua
            .load(
                r#"
                local t = json.decode('{"a":null,"b":1}')
                local s = json.encode(t)
                local t2 = json.decode(s)
                return tostring(t2.b)
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(result, "1");
    }
}
