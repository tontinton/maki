use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, LuaSerdeExt, Result as LuaResult, UserData, UserDataMethods, Value};

use super::util::convert::{err_pair, json_to_lua, lua_to_json};

pub(crate) const VALIDATOR_DOCS: crate::docs::ModuleDoc = crate::docs::ModuleDoc {
    name: "maki.json.SchemaValidator",
    kind: crate::docs::DocKind::Class,
    desc: "A compiled JSON Schema validator. Create one with \
`maki.json.schema_validator()` and reuse it to validate many values \
without recompiling the schema each time.",
    fns: &[crate::docs::FnDoc {
        name: "validate",
        args: "{value}",
        desc: "Check {value} against the compiled schema. Returns nil \
when the value is valid. When validation fails, returns a list of \
human-readable error strings.",
        params: &[crate::docs::ParamDoc {
            name: "{value}",
            ty: "any",
            desc: "The Lua value to validate.",
        }],
        returns: "(table?) Array of error strings, or nil if valid.",
        example: "local errs = validator:validate({ name = 123 })\n\
if errs then\n\
  for _, msg in ipairs(errs) do print(msg) end\n\
end",
    }],
};

/// Schema compile errors surface at creation time, before spending any tokens.
struct LuaSchemaValidator {
    validator: jsonschema::Validator,
}

impl UserData for LuaSchemaValidator {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("validate", |lua, this, value: Value| {
            let json = lua_to_json(lua, &value)?;
            let errors: Vec<String> = this
                .validator
                .iter_errors(&json)
                .map(|e| {
                    let path = e.instance_path.to_string();
                    if path.is_empty() {
                        e.to_string()
                    } else {
                        format!("at {path}: {e}")
                    }
                })
                .collect();
            if errors.is_empty() {
                return Ok(Value::Nil);
            }
            let tbl = lua.create_table()?;
            for (i, err) in errors.into_iter().enumerate() {
                tbl.set(i + 1, err)?;
            }
            Ok(Value::Table(tbl))
        });
    }
}

/// Turn a Lua value into a JSON string. Tables, strings, numbers,
/// booleans, and nil all work. Functions and userdata cannot be
/// serialized.
///
/// @param value any Lua value to encode.
/// @return (string?, string?) JSON string, or nil plus an error.
/// @example
/// local s, err = maki.json.encode({ name = "maki", version = 1 })
/// print(s) -- {"name":"maki","version":1}
#[lua_fn]
fn encode(lua: &Lua, value: Value) -> LuaResult<(Value, Value)> {
    let serde_val: serde_json::Value = match lua.from_value(value) {
        Ok(v) => v,
        Err(e) => return err_pair(lua, e),
    };
    match serde_json::to_string(&serde_val) {
        Ok(s) => Ok((Value::String(lua.create_string(&s)?), Value::Nil)),
        Err(e) => err_pair(lua, e),
    }
}

/// Parse a JSON string into a Lua value. Objects become tables and
/// arrays become 1-indexed sequences.
///
/// @param str string JSON string to decode.
/// @return (any?, string?) Decoded value, or nil plus an error.
/// @example
/// local t, err = maki.json.decode('{"x": 42}')
/// print(t.x) -- 42
#[lua_fn]
fn decode(lua: &Lua, str: String) -> LuaResult<(Value, Value)> {
    match serde_json::from_str::<serde_json::Value>(&str) {
        Ok(v) => Ok((json_to_lua(lua, &v)?, Value::Nil)),
        Err(e) => err_pair(lua, e),
    }
}

/// Compile a JSON Schema into a reusable validator object. Supports
/// draft-07, 2019-09, and 2020-12. Schema errors show up right away so
/// you catch mistakes before doing any real work.
///
/// @param schema table JSON Schema as a Lua table.
/// @return (maki.json.SchemaValidator?, string?) Validator, or nil plus an error.
/// @example
/// local v, err = maki.json.schema_validator({
///   type = "object",
///   properties = { name = { type = "string" } },
///   required = { "name" },
/// })
/// local errs = v:validate({ name = "maki" })
/// assert(errs == nil)
#[lua_fn]
fn schema_validator(lua: &Lua, schema: Value) -> LuaResult<(Value, Value)> {
    let schema_json = match lua_to_json(lua, &schema) {
        Ok(v) => v,
        Err(e) => return err_pair(lua, e),
    };
    match jsonschema::validator_for(&schema_json) {
        Ok(validator) => Ok((
            Value::UserData(lua.create_userdata(LuaSchemaValidator { validator })?),
            Value::Nil,
        )),
        Err(e) => err_pair(lua, e),
    }
}

lua_table! {
    /// JSON encoding, decoding, and schema validation. Encode Lua
    /// tables to JSON strings, decode JSON back into tables, and
    /// optionally validate data against a JSON Schema.
    ///
    /// ```lua
    /// local s = maki.json.encode({ ok = true })
    /// local t = maki.json.decode(s)
    /// ```
    "maki.json" => pub(crate) fn create_json_table(), DOCS [
        encode, decode, schema_validator,
    ]
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
    fn encode_decode_empty_array_roundtrips() {
        let lua = lua_with_json();
        let result: String = lua
            .load(r#"local s = json.encode(json.decode('[]')); return s"#)
            .eval()
            .unwrap();
        assert_eq!(result, "[]");
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
