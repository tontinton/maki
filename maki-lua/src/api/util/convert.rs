use mlua::{Lua, Result as LuaResult, Value};
use serde_json::Value as JsonValue;

pub(crate) fn err_pair(lua: &Lua, e: impl std::fmt::Display) -> LuaResult<(Value, Value)> {
    Ok((Value::Nil, Value::String(lua.create_string(e.to_string())?)))
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
pub(crate) fn lua_to_json(val: &Value) -> LuaResult<JsonValue> {
    Ok(match val {
        Value::Nil => JsonValue::Null,
        Value::Boolean(b) => JsonValue::Bool(*b),
        Value::Integer(n) => JsonValue::Number((*n).into()),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        Value::String(s) => JsonValue::String(s.to_str()?.to_owned()),
        Value::Table(tbl) => {
            let len = tbl.raw_len();
            if len > 0 {
                let mut arr = Vec::with_capacity(len);
                for i in 1..=len {
                    let v: Value = tbl.raw_get(i)?;
                    arr.push(lua_to_json(&v)?);
                }
                JsonValue::Array(arr)
            } else {
                let mut map = serde_json::Map::new();
                for pair in tbl.pairs::<String, Value>() {
                    let (k, v) = pair?;
                    map.insert(k, lua_to_json(&v)?);
                }
                JsonValue::Object(map)
            }
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
        let result = lua_to_json(&input).unwrap();
        assert_eq!(result, expected);
    }

    #[test_case(f64::NAN ; "nan")]
    #[test_case(f64::INFINITY ; "positive_infinity")]
    #[test_case(f64::NEG_INFINITY ; "negative_infinity")]
    fn lua_to_json_non_finite_floats_become_null(n: f64) {
        let result = lua_to_json(&Value::Number(n)).unwrap();
        assert_eq!(result, JsonValue::Null);
    }

    #[test_case(i64::MAX ; "i64_max")]
    #[test_case(i64::MIN ; "i64_min")]
    #[test_case(0 ; "zero")]
    fn lua_to_json_integer_boundaries(n: i64) {
        let result = lua_to_json(&Value::Integer(n)).unwrap();
        assert_eq!(result, serde_json::json!(n));
    }

    #[test]
    fn lua_to_json_string() {
        let lua = Lua::new();
        let s = lua.create_string("hello").unwrap();
        let result = lua_to_json(&Value::String(s)).unwrap();
        assert_eq!(result, serde_json::json!("hello"));
    }

    #[test]
    fn lua_to_json_array_table() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set(1, 10).unwrap();
        tbl.raw_set(2, 20).unwrap();
        tbl.raw_set(3, 30).unwrap();

        let result = lua_to_json(&Value::Table(tbl)).unwrap();
        assert_eq!(result, serde_json::json!([10, 20, 30]));
    }

    #[test]
    fn lua_to_json_object_table() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.set("key", "value").unwrap();

        let result = lua_to_json(&Value::Table(tbl)).unwrap();
        assert_eq!(result, serde_json::json!({"key": "value"}));
    }

    #[test]
    fn lua_to_json_empty_table_is_empty_object() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();

        let result = lua_to_json(&Value::Table(tbl)).unwrap();
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

        let result = lua_to_json(&Value::Table(outer)).unwrap();
        assert_eq!(result, serde_json::json!({"items": [1, {"z": true}]}));
    }

    #[test]
    fn lua_to_json_array_with_hole_reads_up_to_raw_len() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set(1, "a").unwrap();
        tbl.raw_set(2, Value::Nil).unwrap();
        tbl.raw_set(3, "c").unwrap();

        let len = tbl.raw_len();
        let result = lua_to_json(&Value::Table(tbl)).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), len);
    }

    #[test]
    fn lua_to_json_function_becomes_null() {
        let lua = Lua::new();
        let func = lua.create_function(|_, ()| Ok(())).unwrap();
        let result = lua_to_json(&Value::Function(func)).unwrap();
        assert_eq!(result, JsonValue::Null);
    }

    #[test]
    fn lua_to_json_thread_becomes_null() {
        let lua = Lua::new();
        let thread = lua
            .create_thread(lua.create_function(|_, ()| Ok(())).unwrap())
            .unwrap();
        let result = lua_to_json(&Value::Thread(thread)).unwrap();
        assert_eq!(result, JsonValue::Null);
    }

    const ROUNDTRIP_CASES: &[&str] = &[
        "null",
        "true",
        "42",
        "3.14",
        r#""hello""#,
        "[1,2,3]",
        r#"{"a":1,"b":[true,"x"]}"#,
    ];

    #[test_case(0 ; "null")]
    #[test_case(1 ; "bool")]
    #[test_case(2 ; "integer")]
    #[test_case(3 ; "float")]
    #[test_case(4 ; "string")]
    #[test_case(5 ; "array")]
    #[test_case(6 ; "nested_object")]
    fn lua_to_json_roundtrip(idx: usize) {
        let original: JsonValue = serde_json::from_str(ROUNDTRIP_CASES[idx]).unwrap();
        let lua = Lua::new();
        let lua_val = json_to_lua(&lua, &original).unwrap();
        let back = lua_to_json(&lua_val).unwrap();
        assert_eq!(back, original);
    }
}
