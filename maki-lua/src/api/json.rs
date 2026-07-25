use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, LuaSerdeExt, Result as LuaResult, UserData, UserDataMethods, Value};
use serde::{Deserialize, Serialize};

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

/// Encode a Lua value as TOON (Token-Oriented Object Notation), a token-efficient
/// alternative to JSON for LLM context (~30-60% fewer tokens on uniform arrays of
/// objects). Opt-in: pair with `from_toon` only when the consumer is a model.
///
/// @param value any Lua value to encode.
/// @return (string?, string?) TOON string, or nil plus an error.
/// @example
/// local s, err = maki.json.to_toon({ users = { { id = 1, name = "Alice" } } })
#[lua_fn]
fn to_toon(lua: &Lua, value: Value) -> LuaResult<(Value, Value)> {
    let serde_val: serde_json::Value = match lua.from_value(value) {
        Ok(v) => v,
        Err(e) => return err_pair(lua, e),
    };
    match toon_format::encode_default(&serde_val) {
        Ok(s) => Ok((Value::String(lua.create_string(&s)?), Value::Nil)),
        Err(e) => err_pair(lua, e),
    }
}

/// Decode a TOON string back into a Lua value. Inverse of `to_toon`.
///
/// @param str string TOON string to decode.
/// @return (any?, string?) Decoded value, or nil plus an error.
/// @example
/// local t, err = maki.json.from_toon(s)
#[lua_fn]
fn from_toon(lua: &Lua, str: String) -> LuaResult<(Value, Value)> {
    match toon_format::decode_default::<serde_json::Value>(&str) {
        Ok(v) => Ok((json_to_lua(lua, &v)?, Value::Nil)),
        Err(e) => err_pair(lua, e),
    }
}

#[derive(Default, Serialize, Deserialize, Debug, Clone)]
struct ToonStats {
    calls: u64,
    json_bytes: u64,
    toon_bytes: u64,
    toon_wins: u64,
    saved_bytes: u64,
}

fn toon_stats_path() -> Option<PathBuf> {
    maki_storage::paths::data_dir()
        .ok()
        .map(|dir| dir.join("toon_stats.json"))
}

fn load_toon_stats() -> ToonStats {
    if let Some(path) = toon_stats_path()
        && let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(stats) = serde_json::from_str::<ToonStats>(&text)
    {
        return stats;
    }
    ToonStats::default()
}

fn record_toon_stats(json_len: usize, toon_len: usize, used_toon: bool) {
    static STATS: OnceLock<Mutex<ToonStats>> = OnceLock::new();
    let guard = STATS.get_or_init(|| Mutex::new(load_toon_stats()));
    if let Ok(mut stats) = guard.lock() {
        stats.calls += 1;
        stats.json_bytes += json_len as u64;
        stats.toon_bytes += toon_len as u64;
        if used_toon {
            stats.toon_wins += 1;
            stats.saved_bytes += json_len.saturating_sub(toon_len) as u64;
        }
        if let Some(path) = toon_stats_path()
            && let Ok(bytes) = serde_json::to_vec(&*stats)
        {
            let _ = maki_storage::atomic_write(&path, &bytes);
        }
    }
}

/// Lossless JSON/TOON passthrough. Encodes the value as JSON and TOON and
/// returns whichever representation is smaller. If TOON does not shrink the
/// payload, the original JSON string is returned unchanged.
///
/// @param value any Lua value to encode.
/// @return (string?, string?) Encoded string (JSON or TOON) and its format ("json" or "toon"), or nil plus an error.
/// @example
/// local s, fmt = maki.json.tooned({ users = { { id = 1, name = "Alice" } } })
#[lua_fn]
fn tooned(lua: &Lua, value: Value) -> LuaResult<(Value, Value)> {
    let serde_val: serde_json::Value = match lua.from_value(value) {
        Ok(v) => v,
        Err(e) => return err_pair(lua, e),
    };
    let json = match serde_json::to_string(&serde_val) {
        Ok(s) => s,
        Err(e) => return err_pair(lua, e),
    };
    let toon = match toon_format::encode_default(&serde_val) {
        Ok(s) => s,
        Err(e) => return err_pair(lua, e),
    };
    let use_toon = toon.len() < json.len()
        && toon_format::decode_default::<serde_json::Value>(&toon)
            .ok()
            .and_then(|decoded| serde_json::to_value(&decoded).ok())
            .is_some_and(|decoded| decoded == serde_val);
    record_toon_stats(json.len(), toon.len(), use_toon);
    if use_toon {
        Ok((Value::String(lua.create_string(&toon)?), Value::String(lua.create_string("toon")?)))
    } else {
        Ok((Value::String(lua.create_string(&json)?), Value::String(lua.create_string("json")?)))
    }
}

/// Return historical TOON passthrough statistics.
///
/// @return (table?, string?) Stats table with calls, json_bytes, toon_bytes, toon_wins, saved_bytes, or nil plus an error.
/// @example
/// local stats, err = maki.json.toon_stats()
#[lua_fn]
fn toon_stats(lua: &Lua) -> LuaResult<(Value, Value)> {
    let stats = load_toon_stats();
    let tbl = lua.create_table()?;
    tbl.set("calls", stats.calls)?;
    tbl.set("json_bytes", stats.json_bytes)?;
    tbl.set("toon_bytes", stats.toon_bytes)?;
    tbl.set("toon_wins", stats.toon_wins)?;
    tbl.set("saved_bytes", stats.saved_bytes)?;
    Ok((Value::Table(tbl), Value::Nil))
}

lua_table! {
    /// JSON encoding, decoding, schema validation, and TOON round-trip.
    /// Encode Lua tables to JSON strings, decode JSON back into tables,
    /// validate against a JSON Schema, or convert to/from TOON for
    /// token-efficient context blocks.
    ///
    /// ```lua
    /// local s = maki.json.encode({ ok = true })
    /// local t = maki.json.decode(s)
    /// ```
    "maki.json" => pub(crate) fn create_json_table(), DOCS [
        encode, decode, schema_validator, to_toon, from_toon, tooned, toon_stats,
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
