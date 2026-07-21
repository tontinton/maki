use mlua::{Lua, LuaSerdeExt, Result as LuaResult, Value};
use serde_json::Value as JsonValue;

pub(crate) fn err_pair(lua: &Lua, e: impl std::fmt::Display) -> LuaResult<(Value, Value)> {
    Ok((Value::Nil, Value::String(lua.create_string(e.to_string())?)))
}

pub(crate) const NIL_TOOL_RESULT_ERR: &str = "tool returned nil without an error message";

pub(crate) fn lua_tool_result(values: mlua::MultiValue) -> Result<String, String> {
    let mut iter = values.into_iter();
    match iter.next() {
        Some(Value::String(s)) => Ok(s.to_string_lossy()),
        Some(Value::Nil) | None => match iter.next() {
            Some(Value::String(err)) => Err(err.to_string_lossy()),
            _ => Err(NIL_TOOL_RESULT_ERR.into()),
        },
        Some(other) => Err(format!(
            "tool returned {} (expected string)",
            other.type_name()
        )),
    }
}

/// Convert a [`serde_json::Value`] into a Lua value by hand.
///
/// mlua's `to_value` looks like the easy path, but monty turns on serde_json's
/// `arbitrary_precision` feature for the whole workspace. With it, a number
/// serializes as a little tagged struct instead of a plain scalar, so plugins
/// end up with a Lua table where they asked for a number. We walk the tree
/// ourselves to keep numbers as numbers.
pub(crate) fn json_to_lua(lua: &Lua, value: &JsonValue) -> LuaResult<Value> {
    Ok(match value {
        JsonValue::Null => Value::Nil,
        JsonValue::Bool(b) => Value::Boolean(*b),
        JsonValue::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Value::Integer(i),
            (_, Some(f)) => Value::Number(f),
            _ => Value::Nil,
        },
        JsonValue::String(s) => Value::String(lua.create_string(s)?),
        JsonValue::Array(items) => {
            let table = lua.create_table_with_capacity(items.len(), 0)?;
            for (idx, item) in items.iter().enumerate() {
                table.set(idx + 1, json_to_lua(lua, item)?)?;
            }
            table.set_metatable(Some(lua.array_metatable()))?;
            Value::Table(table)
        }
        JsonValue::Object(map) => {
            let table = lua.create_table_with_capacity(0, map.len())?;
            for (key, val) in map {
                table.set(key.as_str(), json_to_lua(lua, val)?)?;
            }
            Value::Table(table)
        }
    })
}

/// Convert a Lua value into a [`serde_json::Value`] by hand.
///
/// Symmetric counterpart to [`json_to_lua`]. We avoid mlua's `from_value`
/// for the same `arbitrary_precision` reason documented above.
pub(crate) fn lua_to_json(lua: &Lua, val: &Value) -> LuaResult<JsonValue> {
    Ok(match val {
        Value::Nil => JsonValue::Null,
        Value::Boolean(b) => JsonValue::Bool(*b),
        Value::Integer(n) => JsonValue::Number((*n).into()),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        Value::String(s) => JsonValue::String(s.to_str()?.to_owned()),
        Value::Table(tbl) => {
            // A table serializes as a JSON array only when every key is a
            // positive integer and they are dense from 1 (count == max), so no
            // string key silently disappears and sparse tables like
            // `{ [1] = "a", [3] = "c" }` deterministically become objects
            // (`lua_rawlen` borders are implementation-defined for those).
            let mut has_non_int = false;
            let mut int_count = 0;
            let mut max_int = 0;
            let mut entries = Vec::new();
            for pair in tbl.pairs::<Value, Value>() {
                let (k, v) = pair?;
                match k {
                    Value::Integer(i) if i > 0 => {
                        int_count += 1;
                        max_int = max_int.max(i as usize);
                    }
                    _ => has_non_int = true,
                }
                entries.push((k, v));
            }

            let is_array = !has_non_int
                && int_count == max_int
                && (int_count > 0 || tbl.metatable().as_ref() == Some(&lua.array_metatable()));
            if is_array {
                let mut arr = vec![JsonValue::Null; int_count];
                for (k, v) in entries {
                    let Value::Integer(i) = k else { unreachable!() };
                    arr[i as usize - 1] = lua_to_json(lua, &v)?;
                }
                return Ok(JsonValue::Array(arr));
            }

            let mut map = serde_json::Map::new();
            for (k, v) in entries {
                let key = match k {
                    Value::String(s) => s.to_str()?.to_owned(),
                    Value::Integer(i) => i.to_string(),
                    Value::Number(n) => n.to_string(),
                    Value::Boolean(b) => b.to_string(),
                    _ => continue,
                };
                map.insert(key, lua_to_json(lua, &v)?);
            }
            JsonValue::Object(map)
        }
        _ => JsonValue::Null,
    })
}

#[cfg(test)]
mod tests {
    use mlua::{Lua, Value};
    use serde_json::Value as JsonValue;
    use test_case::test_case;

    use super::{json_to_lua, lua_to_json};

    #[test_case(Value::Nil, JsonValue::Null ; "nil_to_null")]
    #[test_case(Value::Boolean(true), JsonValue::Bool(true) ; "bool_true")]
    #[test_case(Value::Boolean(false), JsonValue::Bool(false) ; "bool_false")]
    #[test_case(Value::Integer(42), serde_json::json!(42) ; "integer")]
    #[test_case(Value::Number(1.5), serde_json::json!(1.5) ; "float")]
    fn lua_to_json_scalars(input: Value, expected: JsonValue) {
        let lua = Lua::new();
        let result = lua_to_json(&lua, &input).unwrap();
        assert_eq!(result, expected);
    }

    #[test_case(f64::NAN ; "nan")]
    #[test_case(f64::INFINITY ; "positive_infinity")]
    #[test_case(f64::NEG_INFINITY ; "negative_infinity")]
    fn lua_to_json_non_finite_floats_become_null(n: f64) {
        let lua = Lua::new();
        let result = lua_to_json(&lua, &Value::Number(n)).unwrap();
        assert_eq!(result, JsonValue::Null);
    }

    #[test_case(i64::MAX ; "i64_max")]
    #[test_case(i64::MIN ; "i64_min")]
    #[test_case(0 ; "zero")]
    fn lua_to_json_integer_boundaries(n: i64) {
        let lua = Lua::new();
        let result = lua_to_json(&lua, &Value::Integer(n)).unwrap();
        assert_eq!(result, serde_json::json!(n));
    }

    #[test]
    fn lua_to_json_string() {
        let lua = Lua::new();
        let s = lua.create_string("hello").unwrap();
        let result = lua_to_json(&lua, &Value::String(s)).unwrap();
        assert_eq!(result, serde_json::json!("hello"));
    }

    #[test]
    fn lua_to_json_array_table() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set(1, 10).unwrap();
        tbl.raw_set(2, 20).unwrap();
        tbl.raw_set(3, 30).unwrap();

        let result = lua_to_json(&lua, &Value::Table(tbl)).unwrap();
        assert_eq!(result, serde_json::json!([10, 20, 30]));
    }

    #[test]
    fn lua_to_json_object_table() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.set("key", "value").unwrap();

        let result = lua_to_json(&lua, &Value::Table(tbl)).unwrap();
        assert_eq!(result, serde_json::json!({"key": "value"}));
    }

    #[test]
    fn lua_to_json_empty_table_is_empty_object() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();

        let result = lua_to_json(&lua, &Value::Table(tbl)).unwrap();
        assert_eq!(result, serde_json::json!({}));
    }

    #[test]
    fn lua_to_json_nested_table() {
        let lua = Lua::new();

        let inner_obj = lua.create_table().unwrap();
        inner_obj.set("z", true).unwrap();

        let inner_arr = lua.create_table().unwrap();
        inner_arr.raw_set(1, 1).unwrap();
        inner_arr.raw_set(2, inner_obj).unwrap();

        let outer = lua.create_table().unwrap();
        outer.set("items", inner_arr).unwrap();

        let result = lua_to_json(&lua, &Value::Table(outer)).unwrap();
        assert_eq!(result, serde_json::json!({"items": [1, {"z": true}]}));
    }

    #[test]
    fn lua_to_json_sparse_table_becomes_object() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set(1, "a").unwrap();
        tbl.raw_set(3, "c").unwrap();

        let result = lua_to_json(&lua, &Value::Table(tbl)).unwrap();
        assert_eq!(result, serde_json::json!({"1": "a", "3": "c"}));
    }

    #[test]
    fn lua_to_json_array_metatable_with_string_key_becomes_object() {
        let lua = Lua::new();
        let arr = json_to_lua(&lua, &serde_json::json!([10, 20])).unwrap();
        let tbl = arr.as_table().unwrap();
        tbl.set("total", 5).unwrap();

        let result = lua_to_json(&lua, &arr).unwrap();
        assert_eq!(result, serde_json::json!({"1": 10, "2": 20, "total": 5}));
    }

    #[test]
    fn lua_to_json_mixed_table_becomes_object() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set(1, "first").unwrap();
        tbl.set("pattern", "grep").unwrap();

        let result = lua_to_json(&lua, &Value::Table(tbl)).unwrap();
        assert_eq!(result, serde_json::json!({"1": "first", "pattern": "grep"}));
    }

    #[test]
    fn lua_to_json_function_becomes_null() {
        let lua = Lua::new();
        let func = lua.create_function(|_, ()| Ok(())).unwrap();
        let result = lua_to_json(&lua, &Value::Function(func)).unwrap();
        assert_eq!(result, JsonValue::Null);
    }

    #[test]
    fn lua_to_json_thread_becomes_null() {
        let lua = Lua::new();
        let thread = lua
            .create_thread(lua.create_function(|_, ()| Ok(())).unwrap())
            .unwrap();
        let result = lua_to_json(&lua, &Value::Thread(thread)).unwrap();
        assert_eq!(result, JsonValue::Null);
    }

    const ROUNDTRIP_CASES: &[&str] = &[
        "null",
        "true",
        "42",
        "3.14",
        r#""hello""#,
        "[1,2,3]",
        "[]",
        r#"{}"#,
        r#"{"a":1,"b":[true,"x"]}"#,
    ];

    #[test_case(0 ; "null")]
    #[test_case(1 ; "bool")]
    #[test_case(2 ; "integer")]
    #[test_case(3 ; "float")]
    #[test_case(4 ; "string")]
    #[test_case(5 ; "array")]
    #[test_case(6 ; "empty_array")]
    #[test_case(7 ; "empty_object")]
    #[test_case(8 ; "nested_object")]
    fn lua_to_json_roundtrip(idx: usize) {
        let original: JsonValue = serde_json::from_str(ROUNDTRIP_CASES[idx]).unwrap();
        let lua = Lua::new();
        let lua_val = json_to_lua(&lua, &original).unwrap();
        let back = lua_to_json(&lua, &lua_val).unwrap();
        assert_eq!(back, original);
    }
}
