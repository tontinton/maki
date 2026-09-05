use mlua::{Lua, LuaSerdeExt, Result as LuaResult, Value};
use serde_json::Value as JsonValue;

pub(crate) const NIL_TOOL_RESULT_ERR: &str = "tool returned nil without an error message";

/// How many nulls an array encoding may invent for keys the table does not
/// hold. A JSON null arrives as an absent Lua key, so a round trip has to be
/// able to put a few back, but nothing bounds the largest key a table carries.
/// Past this the table is a sparse map, not an array, and the object encoding
/// keeps every key while allocating per entry.
const MAX_ARRAY_HOLES: usize = 4096;

pub fn lua_tool_result(values: mlua::MultiValue) -> Result<String, String> {
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
pub fn json_to_lua(lua: &Lua, value: &JsonValue) -> LuaResult<Value> {
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
pub fn lua_to_json(lua: &Lua, val: &Value) -> LuaResult<JsonValue> {
    within_template(lua, val, None)
}

/// [`lua_to_json`], guided by the JSON that [`json_to_lua`] built `val` from:
/// whatever the Lua side produced wins, and a null the Lua side left absent is
/// restored from `template`.
///
/// A JSON null arrives as a Lua nil, and assigning nil to a table key is a
/// no-op, so a layer never sees a null and cannot hand one back. They have to
/// be carried across instead. Guiding stops wherever the two sides stop being
/// the same container kind, so a layer that swapped a subtree for a scalar owns
/// it outright. The price: a layer cannot delete a key whose value is null, it
/// comes back, while deleting a key holding a real value works.
pub(crate) fn lua_to_json_within(
    lua: &Lua,
    val: &Value,
    template: &JsonValue,
) -> LuaResult<JsonValue> {
    within_template(lua, val, Some(template))
}

fn within_template(lua: &Lua, val: &Value, template: Option<&JsonValue>) -> LuaResult<JsonValue> {
    Ok(match val {
        Value::Nil => JsonValue::Null,
        Value::Boolean(b) => JsonValue::Bool(*b),
        Value::Integer(n) => JsonValue::Number((*n).into()),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        Value::String(s) => JsonValue::String(s.to_str()?.to_owned()),
        Value::Table(tbl) => {
            // An untagged table serializes as a JSON array only when every key
            // is a positive integer and they are dense from 1 (count == max),
            // so no string key silently disappears and sparse tables like
            // `{ [1] = "a", [3] = "c" }` deterministically become objects
            // (`lua_rawlen` borders are implementation-defined for those).
            // The array metatable outranks that, since `json_to_lua` writes it
            // on every JSON array and a null element leaves an absent key:
            // density cannot be re-derived, and the gaps are holes to fill with
            // null. Keys an array cannot express still fall back to the object
            // encoding, which keeps all of them.
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

            let tagged = tbl.metatable().as_ref() == Some(&lua.array_metatable());
            // Slots the entries do not pay for. The array encoding has to
            // materialize every one of them, and the largest key alone decides
            // how many, so this is the only thing standing between
            // `decoded[os.time()] = 1` and an allocation the size of a clock.
            let holes = max_int - int_count;
            let is_array = !has_non_int
                && if tagged {
                    holes <= MAX_ARRAY_HOLES
                } else {
                    int_count > 0 && holes == 0
                };
            if is_array {
                let template = template.and_then(JsonValue::as_array);
                // A trailing null never moved `max_int`, so only the template
                // knows the array ran on past the last key the Lua side kept.
                // A trailing non-null there was a real value the layer dropped.
                let mut len = max_int;
                while template.is_some_and(|t| t.get(len).is_some_and(JsonValue::is_null)) {
                    len += 1;
                }
                let mut arr = vec![JsonValue::Null; len];
                for (k, v) in entries {
                    let Value::Integer(i) = k else { unreachable!() };
                    let idx = i as usize - 1;
                    arr[idx] = within_template(lua, &v, template.and_then(|t| t.get(idx)))?;
                }
                return Ok(JsonValue::Array(arr));
            }

            let template = template.and_then(JsonValue::as_object);
            let mut map = serde_json::Map::new();
            for (k, v) in entries {
                let key = match k {
                    Value::String(s) => s.to_str()?.to_owned(),
                    Value::Integer(i) => i.to_string(),
                    Value::Number(n) => n.to_string(),
                    Value::Boolean(b) => b.to_string(),
                    _ => continue,
                };
                let child = template.and_then(|t| t.get(&key));
                map.insert(key, within_template(lua, &v, child)?);
            }
            for (key, _) in template.into_iter().flatten().filter(|(_, v)| v.is_null()) {
                map.entry(key.as_str()).or_insert(JsonValue::Null);
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

    use super::{MAX_ARRAY_HOLES, json_to_lua, lua_to_json, lua_to_json_within};

    /// The name `LAYER_CASES` snippets edit through, standing in for the
    /// `value` argument a real hook layer is handed.
    const LAYER_GLOBAL: &str = "value";

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

    /// The array encoding materializes every hole, and the largest key alone
    /// says how many, so a table used as a sparse map has to leave the encoding
    /// rather than allocate up to its key. Nothing is lost: the object keeps
    /// every entry.
    #[test_case(MAX_ARRAY_HOLES,     true  ; "holes_the_entries_can_carry")]
    #[test_case(MAX_ARRAY_HOLES + 1, false ; "a_key_too_far_falls_back_to_object")]
    fn lua_to_json_array_encoding_bounds_the_holes_it_invents(holes: usize, array: bool) {
        let lua = Lua::new();
        let value = json_to_lua(&lua, &serde_json::json!([1])).unwrap();
        let key = holes + 2;
        value.as_table().unwrap().set(key, 2).unwrap();

        let result = lua_to_json(&lua, &value).unwrap();
        assert_eq!(result.is_array(), array);
        assert_eq!(
            result.get(key - 1).or_else(|| result.get(key.to_string())),
            Some(&serde_json::json!(2)),
            "either encoding keeps the entry"
        );
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
        "[1,null,3]",
        "[[],{}]",
        r#"{"a":[1,2,null,3]}"#,
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
    #[test_case(9 ; "array_with_interior_null")]
    #[test_case(10 ; "nested_empty_containers")]
    #[test_case(11 ; "object_holding_array_with_null")]
    fn lua_to_json_roundtrip(idx: usize) {
        let original: JsonValue = serde_json::from_str(ROUNDTRIP_CASES[idx]).unwrap();
        let lua = Lua::new();
        let lua_val = json_to_lua(&lua, &original).unwrap();
        let back = lua_to_json(&lua, &lua_val).unwrap();
        assert_eq!(back, original);
    }

    /// A layer sees no null at all, so a pass-through of any input has to come
    /// back exactly as it went in.
    const TEMPLATE_IDENTITY_CASES: &[&str] = &[
        r#"{"a":null,"b":1}"#,
        "[1,null]",
        "[null]",
        r#"{"a":[1,null]}"#,
        "[1,null,3]",
        "{}",
        "[]",
        r#"{"a":{"b":null,"c":[null,{"d":null},null]},"e":[[null],null],"f":null}"#,
    ];

    #[test_case(0 ; "object_with_null")]
    #[test_case(1 ; "array_with_trailing_null")]
    #[test_case(2 ; "array_of_one_null")]
    #[test_case(3 ; "nested_array_with_trailing_null")]
    #[test_case(4 ; "array_with_interior_null")]
    #[test_case(5 ; "empty_object")]
    #[test_case(6 ; "empty_array")]
    #[test_case(7 ; "deeply_nested_mix")]
    fn lua_to_json_within_template_roundtrips_nulls(idx: usize) {
        let original: JsonValue = serde_json::from_str(TEMPLATE_IDENTITY_CASES[idx]).unwrap();
        let lua = Lua::new();
        let lua_val = json_to_lua(&lua, &original).unwrap();

        let back = lua_to_json_within(&lua, &lua_val, &original).unwrap();
        assert_eq!(back, original);
    }

    /// `(template, what the layer does to it, what the caller must get back)`.
    const LAYER_CASES: &[(&str, &str, &str)] = &[
        ("[1,2,3]", "value[3] = nil", "[1,2]"),
        (
            r#"{"a":null,"b":1}"#,
            r#"value.a = "x""#,
            r#"{"a":"x","b":1}"#,
        ),
        (
            r#"{"a":{"b":null},"c":2}"#,
            "value.a = 1",
            r#"{"a":1,"c":2}"#,
        ),
        (r#"{"a":null,"b":1}"#, "value.b = nil", r#"{"a":null}"#),
    ];

    #[test_case(0 ; "truncating_a_real_value_shortens_the_array")]
    #[test_case(1 ; "a_null_replaced_by_a_value_keeps_the_value")]
    #[test_case(2 ; "an_object_replaced_by_a_scalar_ends_the_template")]
    #[test_case(3 ; "deleting_a_non_null_key_still_deletes_it")]
    fn lua_to_json_within_template_lets_the_layer_win(idx: usize) {
        let (template, edit, expected) = LAYER_CASES[idx];
        let template: JsonValue = serde_json::from_str(template).unwrap();
        let lua = Lua::new();
        let lua_val = json_to_lua(&lua, &template).unwrap();
        lua.globals().set(LAYER_GLOBAL, lua_val.clone()).unwrap();
        lua.load(edit).exec().unwrap();

        let result = lua_to_json_within(&lua, &lua_val, &template).unwrap();
        assert_eq!(result, serde_json::from_str::<JsonValue>(expected).unwrap());
    }
}
