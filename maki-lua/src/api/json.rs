use maki_lua_macro::{lua_fn, lua_table};
use mlua::{
    AnyUserData, Lua, LuaString, Result as LuaResult, Table, UserData, UserDataMethods, Value,
};

use super::util::convert::{Luanil, TableExt, json_to_lua_with, lua_to_json};
use super::util::pair::{Pair, pair, try_pair};

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
        guard: None,
        value: false,
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
                    let path = e.instance_path().to_string();
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
/// booleans, nil, and `maki.NIL` (which becomes null) all work. A
/// `maki.image.Image` becomes `{ type = "base64", media_type, data }`.
/// Functions, threads, other userdata, and tables that contain themselves
/// cannot be encoded.
///
/// A table is an array when it came from a JSON array (`maki.json.decode`,
/// `maki.deepcopy` of one), or when all its keys are positive integers and
/// its largest key is at most 10 or at most twice the number of keys, with
/// null in the gaps like `vim.json.encode`. Any other table is an object,
/// with number and boolean keys written as strings.
///
/// @param value any Lua value to encode.
/// @return (string?, string?) JSON string, or nil plus an error.
/// @example
/// local s, err = maki.json.encode({ name = "maki", version = 1 })
/// print(s) -- {"name":"maki","version":1}
#[lua_fn]
fn encode(lua: &Lua, value: Value) -> LuaResult<Pair<String>> {
    let json = try_pair!(lua_to_json(lua, &value));
    Ok(pair(serde_json::to_string(&json)))
}

/// Parse a JSON string into a Lua value. Objects become tables and
/// arrays become 1-indexed sequences. Every JSON null becomes `maki.NIL`, so
/// a key holding null is still there and encoding the result gives the same
/// JSON back. `maki.NIL` is a special value that stands for JSON null. It is
/// truthy, so test for null with `x == maki.NIL`.
///
/// Pass `opts.luanil` to get plain nil instead, like `vim.json.decode`.
/// `luanil.object` does so for nulls inside objects and `luanil.array` for
/// nulls inside arrays. A null at the top level is always `maki.NIL`.
///
/// @param str string JSON string to decode.
/// @param opts table? Optional. `luanil` (table with boolean `object` and `array`) turns those nulls into nil.
/// @return (any?, string?) Decoded value, or nil plus an error.
/// @example
/// local t, err = maki.json.decode('{"x": 42, "y": null}')
/// print(t.x) -- 42
/// print(t.y == maki.NIL) -- true
///
/// local u = maki.json.decode('{"y": null}', { luanil = { object = true } })
/// print(u.y) -- nil
#[lua_fn]
fn decode(lua: &Lua, str: LuaString, opts: Option<Table>) -> LuaResult<Pair<Value>> {
    let luanil = match opts {
        Some(opts) => opts.opt::<Table>("luanil")?,
        None => None,
    };
    let luanil = luanil.map_or(Luanil::NONE, |luanil| Luanil {
        object: luanil.opt_bool("object").unwrap_or(false),
        array: luanil.opt_bool("array").unwrap_or(false),
    });
    let value = try_pair!(serde_json::from_slice::<serde_json::Value>(&str.as_bytes()));
    Ok((Some(json_to_lua_with(lua, &value, &luanil)?), None))
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
fn schema_validator(lua: &Lua, schema: Value) -> LuaResult<Pair<AnyUserData>> {
    let schema_json = try_pair!(lua_to_json(lua, &schema));
    let validator = try_pair!(jsonschema::validator_for(&schema_json));
    Ok((
        Some(lua.create_userdata(LuaSchemaValidator { validator })?),
        None,
    ))
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
    use std::sync::Arc;

    use maki_providers::{ImageMediaType, ImageSource};
    use mlua::{Lua, Value};
    use test_case::test_case;

    use crate::api::image::LuaImage;
    use crate::api::util::convert::UNSUPPORTED_JSON_VALUE_ERR;

    fn lua_with_json() -> Lua {
        let lua = Lua::new();
        let json = super::create_json_table(&lua).unwrap();
        lua.globals().set("json", json).unwrap();
        lua.globals().set("NIL", Value::NULL).unwrap();
        lua
    }

    fn encoded(lua: &Lua, chunk: &str) -> serde_json::Value {
        let json: String = lua.load(chunk).eval().unwrap();
        serde_json::from_str(&json).unwrap()
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

    #[test_case("json.encode" ; "function")]
    #[test_case("{ handler = json.encode }" ; "nested_function")]
    fn encode_error_returns_nil_and_message(value: &str) {
        let lua = lua_with_json();
        let (is_nil, err): (bool, String) = lua
            .load(format!(
                "local s, err = json.encode({value}); return s == nil, err"
            ))
            .eval()
            .unwrap();
        assert!(is_nil);
        assert!(err.contains(UNSUPPORTED_JSON_VALUE_ERR), "got: {err}");
    }

    #[test]
    fn encode_gives_an_image_its_storage_shape() {
        let lua = lua_with_json();
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("iVBORw0KGgo="));
        lua.globals()
            .set("img", LuaImage::from_source(source.clone()))
            .unwrap();
        let back = encoded(
            &lua,
            r#"return json.encode({ type = "image", source = img })"#,
        );
        assert_eq!(
            back,
            serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": ImageMediaType::Png.mime(),
                    "data": &*source.data,
                },
            })
        );
    }

    #[test_case(r#""{invalid}""# ; "invalid_json")]
    #[test_case(r#""\xff""# ; "non_utf8")]
    fn decode_error_returns_nil_and_message(input: &str) {
        let lua = lua_with_json();
        let (is_nil, has_err): (bool, bool) = lua
            .load(format!(
                "local t, err = json.decode({input}); return t == nil, err ~= nil"
            ))
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

    #[test_case(r#"{"a":null,"b":1}"# ; "object_with_null")]
    #[test_case("[1,null,3]" ; "array_with_interior_null")]
    #[test_case("[null]" ; "array_of_one_null")]
    #[test_case("[null,null]" ; "array_of_only_nulls")]
    #[test_case(r#"{"a":[1,null,{"c":null}],"b":[]}"# ; "nested_nulls_and_empty_array")]
    #[test_case(r#"{"input":{"files":[],"limit":null,"tags":[null,"x",null]}}"# ; "tool_use_input")]
    #[test_case("null" ; "top_level_null")]
    fn decode_then_encode_gives_the_same_json(doc: &str) {
        let lua = lua_with_json();
        lua.globals().set("doc", doc).unwrap();
        let back = encoded(&lua, "return json.encode((json.decode(doc)))");
        assert_eq!(
            back,
            serde_json::from_str::<serde_json::Value>(doc).unwrap()
        );
    }

    #[test]
    fn decode_gives_the_sentinel_for_null_by_default() {
        let lua = lua_with_json();
        let (key_kept, is_sentinel): (bool, bool) = lua
            .load(
                r#"
                local t = json.decode('{"a":null}')
                return next(t) == "a", t.a == NIL
                "#,
            )
            .eval()
            .unwrap();
        assert!(key_kept);
        assert!(is_sentinel);
    }

    #[test_case("{ object = true }", true, false ; "object")]
    #[test_case("{ array = true }", false, true ; "array")]
    #[test_case("{ object = true, array = true }", true, true ; "both")]
    #[test_case("{}", false, false ; "empty_luanil")]
    #[test_case("nil", false, false ; "no_luanil")]
    fn decode_luanil_gives_nil_for_the_chosen_containers(
        luanil: &str,
        object_nil: bool,
        array_nil: bool,
    ) {
        let lua = lua_with_json();
        let (object_is_nil, array_is_nil, top_level_is_sentinel): (bool, bool, bool) = lua
            .load(format!(
                r#"
                local opts = {{ luanil = {luanil} }}
                local t = json.decode('{{"o":{{"k":null}},"a":[null]}}', opts)
                return t.o.k == nil, t.a[1] == nil, json.decode('null', opts) == NIL
                "#
            ))
            .eval()
            .unwrap();
        assert_eq!(object_is_nil, object_nil);
        assert_eq!(array_is_nil, array_nil);
        assert!(top_level_is_sentinel);
    }

    #[test_case("NIL" ; "optional_argument")]
    #[test_case("{ luanil = NIL }" ; "optional_table_key")]
    #[test_case("{ luanil = { object = NIL } }" ; "optional_flag_is_not_truthy")]
    fn decode_reads_the_sentinel_as_an_absent_option(opts: &str) {
        let lua = lua_with_json();
        let is_sentinel: bool = lua
            .load(format!(
                r#"return json.decode('{{"a":null}}', {opts}).a == NIL"#
            ))
            .eval()
            .unwrap();
        assert!(is_sentinel);
    }
}
