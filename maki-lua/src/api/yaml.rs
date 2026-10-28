use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, LuaSerdeExt, LuaString, Result as LuaResult, Value};
use serde_saphyr::NonFiniteFloatPolicy;

use super::util::pair::{Pair, pair, try_pair};

/// Turn a Lua value into a YAML string. Most Lua types work, but
/// circular references will return an error.
///
/// @param value any Lua value to encode.
/// @return (string?, string?) YAML string, or nil plus an error.
/// @example
/// local s, err = maki.yaml.encode({ name = "maki", tags = { "ai", "agent" } })
/// print(s)
#[lua_fn]
fn encode(_lua: &Lua, value: Value) -> LuaResult<Pair<String>> {
    Ok(pair(serde_saphyr::to_string(&value)))
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
fn decode(lua: &Lua, str: LuaString) -> LuaResult<Pair<Value>> {
    // The YAML 1.2 core schema: `yes` stays a string and `.inf` is a float.
    let options = serde_saphyr::options! {
        strict_booleans: true,
        non_finite_float_policy: NonFiniteFloatPolicy::PassThrough,
    };
    let value: serde_value::Value = try_pair!(serde_saphyr::from_slice_with_options(
        &str.as_bytes(),
        options
    ));
    Ok((Some(lua.to_value(&value)?), None))
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
    use test_case::test_case;

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

    #[test_case("yes", "string" ; "yaml_1_1_boolean_stays_string")]
    #[test_case("true", "boolean" ; "yaml_1_2_boolean")]
    #[test_case(".inf", "number" ; "infinity_is_a_float")]
    fn decode_scalar_type(scalar: &str, expected: &str) {
        let lua = lua_with_yaml();
        let result: String = lua
            .load(format!("return type(yaml.decode('x: {scalar}').x)"))
            .eval()
            .unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn decode_keeps_integer_keys() {
        let lua = lua_with_yaml();
        let result: String = lua
            .load("return yaml.decode('200: ok')[200]")
            .eval()
            .unwrap();
        assert_eq!(result, "ok");
    }

    #[test_case(r#"":\n  - :\n  bad""# ; "invalid_yaml")]
    #[test_case(r#""\xff""# ; "non_utf8")]
    fn decode_error_returns_nil_and_message(input: &str) {
        let lua = lua_with_yaml();
        let (is_nil, has_err): (bool, bool) = lua
            .load(format!(
                "local t, err = yaml.decode({input}); return t == nil, err ~= nil"
            ))
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
