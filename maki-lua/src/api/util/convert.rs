use std::ffi::c_void;

use mlua::{FromLua, Lua, LuaSerdeExt, Result as LuaResult, Table, Value};
use serde_json::Value as JsonValue;

use crate::api::image::LuaImage;

pub(crate) const NIL_TOOL_RESULT_ERR: &str = "tool returned nil without an error message";
pub(crate) const UNSUPPORTED_JSON_VALUE_ERR: &str = "cannot convert to JSON a value of type";
const UNSUPPORTED_JSON_KEY_ERR: &str = "cannot make a JSON object key from a key of type";
const RECURSIVE_TABLE_ERR: &str = "cannot convert to JSON a table that contains itself";
const TOO_DEEP_ERR: &str = "cannot convert to JSON tables nested deeper than";
pub(crate) const UNKNOWN_FIELD_ERR: &str = "unknown field";

/// How many nulls an array encoding may invent for keys the table does not
/// hold. A JSON null arrives as an absent Lua key, so a round trip has to be
/// able to put a few back, but nothing bounds the largest key a table carries.
/// Past this the table is a sparse map, not an array, and the object encoding
/// keeps every key while allocating per entry.
const MAX_ARRAY_HOLES: usize = 4096;
/// lua-cjson's `encode_sparse_array` defaults, so a table `vim.json.encode`
/// writes as an array is an array here too. Plugins copy that code over.
/// cjson errors on a table sparser than this, we write an object instead.
const SPARSE_ARRAY_SAFE_LEN: usize = 10;
const SPARSE_ARRAY_RATIO: usize = 2;
/// How deep a JSON conversion follows nested tables, so a deep structure
/// fails instead of overflowing the stack. Same as mlua's serde limit.
const MAX_JSON_DEPTH: usize = 128;

/// An `Option<T>` that reads `maki.NIL` as `None`. mlua reads any non-nil
/// value as `Some`, so a field from `maki.json.decode` that held null would
/// count as set, and an `Option<String>` would throw on it. `#[lua_fn]` reads
/// every `Option<T>` parameter through this.
pub(crate) struct NilAsNone<T>(pub(crate) Option<T>);

impl<T: FromLua> FromLua for NilAsNone<T> {
    fn from_lua(value: Value, lua: &Lua) -> LuaResult<Self> {
        if value.is_null() {
            return Ok(Self(None));
        }
        Option::<T>::from_lua(value, lua).map(Self)
    }
}

/// The way the host reads an optional key from a plugin's table. Absent, nil
/// and `maki.NIL` all give `None`, so a table straight from `maki.json.decode`
/// works and `{ focus = maki.NIL }` never reads as `focus = true`.
/// `get::<Option<T>>` and `get::<bool>` miss that, and a test keeps them out.
pub(crate) trait TableExt {
    /// Errors only when the value is there and has the wrong type.
    fn opt<T: FromLua>(&self, key: &str) -> LuaResult<Option<T>>;

    /// mlua reads `bool` by Lua truthiness and never fails, so `get::<bool>`
    /// answers `Ok(false)` for a missing key and quietly kills whatever
    /// `unwrap_or(true)` sat behind it. `Option<bool>` keeps absent apart from
    /// an explicit `false`.
    fn opt_bool(&self, key: &str) -> Option<bool> {
        self.opt(key).ok().flatten()
    }
}

impl TableExt for Table {
    fn opt<T: FromLua>(&self, key: &str) -> LuaResult<Option<T>> {
        Ok(self.get::<NilAsNone<T>>(key)?.0)
    }
}

/// Throws on a key of {table} outside {allowed}, so a typo is an error
/// instead of an option that silently does nothing.
pub(crate) fn reject_unknown_fields(
    table: &Table,
    allowed: &[&str],
    context: &str,
) -> LuaResult<()> {
    for pair in table.pairs::<Value, Value>() {
        let (key, _) = pair?;
        let Value::String(key) = key else {
            return Err(mlua::Error::runtime(format!(
                "{context}: field names must be strings"
            )));
        };
        let key = key.to_str()?;
        if !allowed.contains(&key.as_ref()) {
            return Err(mlua::Error::runtime(format!(
                "{context}: {UNKNOWN_FIELD_ERR} {key:?}"
            )));
        }
    }
    Ok(())
}

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

/// Which JSON nulls become nil instead of [`Value::NULL`] (`maki.NIL`) inside
/// a container, mirroring the `luanil` option of `vim.json.decode`.
pub(crate) struct Luanil {
    pub object: bool,
    pub array: bool,
}

impl Luanil {
    pub(crate) const NONE: Self = Self {
        object: false,
        array: false,
    };
    pub(crate) const ALL: Self = Self {
        object: true,
        array: true,
    };
}

/// Convert a [`serde_json::Value`] into a Lua value for the plugin to
/// consume: every null becomes nil, so a key holding null is simply absent.
///
/// Use this for host-built tables (handler arguments, event payloads, ctx
/// fields, provider tables), where `if opts.limit then` must keep working.
/// Use [`json_to_lua_exact`] for JSON a plugin may hand back to the host.
///
/// Converts by hand rather than with mlua's `to_value`: monty turns on
/// serde_json's `arbitrary_precision` feature for the whole workspace, and with
/// it a number serializes as a little tagged struct instead of a plain scalar,
/// so plugins would get a Lua table where they asked for a number.
pub(crate) fn json_to_lua(lua: &Lua, value: &JsonValue) -> LuaResult<Value> {
    if value.is_null() {
        return Ok(Value::Nil);
    }
    json_to_lua_with(lua, value, &Luanil::ALL)
}

/// [`json_to_lua`], except every null becomes `maki.NIL`, so a Lua → JSON
/// round trip gives back the same JSON.
pub(crate) fn json_to_lua_exact(lua: &Lua, value: &JsonValue) -> LuaResult<Value> {
    json_to_lua_with(lua, value, &Luanil::NONE)
}

/// A top-level null stays `maki.NIL`, as in `vim.json.decode`, since there is
/// no container for `luanil` to apply to.
pub(crate) fn json_to_lua_with(lua: &Lua, value: &JsonValue, luanil: &Luanil) -> LuaResult<Value> {
    Ok(match value {
        JsonValue::Null => Value::NULL,
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
                table.set(idx + 1, element(lua, item, luanil.array, luanil)?)?;
            }
            table.set_metatable(Some(lua.array_metatable()))?;
            Value::Table(table)
        }
        JsonValue::Object(map) => {
            let table = lua.create_table_with_capacity(0, map.len())?;
            for (key, val) in map {
                table.set(key.as_str(), element(lua, val, luanil.object, luanil)?)?;
            }
            Value::Table(table)
        }
    })
}

fn element(lua: &Lua, value: &JsonValue, null_is_nil: bool, luanil: &Luanil) -> LuaResult<Value> {
    if null_is_nil && value.is_null() {
        return Ok(Value::Nil);
    }
    json_to_lua_with(lua, value, luanil)
}

/// Convert a Lua value into a [`serde_json::Value`] by hand.
///
/// Symmetric counterpart to [`json_to_lua`]. We avoid mlua's `from_value`
/// for the same `arbitrary_precision` reason documented above. Both nil and
/// `maki.NIL` become null, and a `maki.image.Image` its storage shape. This is
/// the only Lua → JSON conversion, so `maki.json.encode` and every host reader
/// agree. Anything else JSON cannot hold (functions, threads, other userdata,
/// buffers, tables that contain themselves) is an error, never a silent null.
pub(crate) fn lua_to_json(lua: &Lua, val: &Value) -> LuaResult<JsonValue> {
    within_template(lua, val, None, &mut Vec::new()).map_err(ConvertError::into_lua)
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
    within_template(lua, val, Some(template), &mut Vec::new()).map_err(ConvertError::into_lua)
}

/// A conversion error and the keys leading to the value that failed,
/// innermost first, so the author can find the bad value in a big table.
/// Keys are only added while the error unwinds, so a conversion that
/// succeeds never pays for the path.
struct ConvertError {
    error: mlua::Error,
    path: Vec<String>,
}

impl From<mlua::Error> for ConvertError {
    fn from(error: mlua::Error) -> Self {
        Self {
            error,
            path: Vec::new(),
        }
    }
}

impl ConvertError {
    fn at(mut self, key: &Value) -> Self {
        self.path.push(key_path_segment(key));
        self
    }

    fn into_lua(self) -> mlua::Error {
        if self.path.is_empty() {
            return self.error;
        }
        let message = match self.error {
            mlua::Error::RuntimeError(message) => message,
            other => other.to_string(),
        };
        let path: String = self.path.iter().rev().map(String::as_str).collect();
        mlua::Error::runtime(format!("{message} at {path}"))
    }
}

/// How Lua code would index `key`: `.name` for identifier keys, `[3]` for
/// numbers, and a quoted `["a.b"]` for strings that `.` cannot reach.
fn key_path_segment(key: &Value) -> String {
    match key {
        Value::String(s) => {
            let s = s.to_string_lossy();
            let identifier = s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if identifier {
                format!(".{s}")
            } else {
                format!("[{s:?}]")
            }
        }
        Value::Integer(i) => format!("[{i}]"),
        Value::Number(n) => format!("[{n}]"),
        Value::Boolean(b) => format!("[{b}]"),
        other => format!("[{}]", other.type_name()),
    }
}

/// `ancestors` holds the tables being converted around `val`, so a table
/// that contains itself fails instead of recursing forever.
fn within_template(
    lua: &Lua,
    val: &Value,
    template: Option<&JsonValue>,
    ancestors: &mut Vec<*const c_void>,
) -> Result<JsonValue, ConvertError> {
    Ok(match val {
        Value::Nil => JsonValue::Null,
        _ if val.is_null() => JsonValue::Null,
        Value::Boolean(b) => JsonValue::Bool(*b),
        Value::Integer(n) => JsonValue::Number((*n).into()),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        Value::String(s) => JsonValue::String(s.to_str()?.to_owned()),
        Value::Table(tbl) => {
            let ptr = tbl.to_pointer();
            if ancestors.contains(&ptr) {
                return Err(mlua::Error::runtime(RECURSIVE_TABLE_ERR).into());
            }
            if ancestors.len() >= MAX_JSON_DEPTH {
                return Err(
                    mlua::Error::runtime(format!("{TOO_DEEP_ERR} {MAX_JSON_DEPTH}")).into(),
                );
            }
            ancestors.push(ptr);
            let json = table_to_json(lua, tbl, template, ancestors);
            ancestors.pop();
            json?
        }
        Value::UserData(ud) if ud.is::<LuaImage>() => {
            serde_json::to_value(ud.borrow::<LuaImage>()?.to_source())
                .map_err(mlua::Error::external)?
        }
        other => {
            return Err(mlua::Error::runtime(format!(
                "{UNSUPPORTED_JSON_VALUE_ERR} {}",
                other.type_name()
            ))
            .into());
        }
    })
}

fn table_to_json(
    lua: &Lua,
    tbl: &Table,
    template: Option<&JsonValue>,
    ancestors: &mut Vec<*const c_void>,
) -> Result<JsonValue, ConvertError> {
    // An untagged table serializes as a JSON array only when every key
    // is a positive integer, so no string key silently disappears. Gaps
    // become nulls by lua-cjson's rule, decided by the keys alone since
    // `lua_rawlen` borders are implementation-defined for sparse tables.
    // The array metatable outranks that, since `json_to_lua` writes it
    // on every JSON array and a null element leaves an absent key:
    // density cannot be re-derived, and the gaps are holes to fill with
    // null. Keys an array cannot express still fall back to the object
    // encoding, which keeps all of them. A fresh `{}` cannot say which
    // it is, so the template decides: a layer emptying a list with
    // `value.paths = {}` must not hand back an object.
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
    let is_array = !has_non_int
        && if tagged {
            max_int - int_count <= MAX_ARRAY_HOLES
        } else if int_count == 0 {
            template.is_some_and(JsonValue::is_array)
        } else {
            max_int <= SPARSE_ARRAY_SAFE_LEN || max_int <= SPARSE_ARRAY_RATIO * int_count
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
            arr[idx] = within_template(lua, &v, template.and_then(|t| t.get(idx)), ancestors)
                .map_err(|e| e.at(&k))?;
        }
        return Ok(JsonValue::Array(arr));
    }

    let template = template.and_then(JsonValue::as_object);
    let mut map = serde_json::Map::new();
    for (k, v) in entries {
        let key = match &k {
            Value::String(s) => s.to_str()?.to_owned(),
            Value::Integer(i) => i.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Boolean(b) => b.to_string(),
            other => {
                return Err(mlua::Error::runtime(format!(
                    "{UNSUPPORTED_JSON_KEY_ERR} {}",
                    other.type_name()
                ))
                .into());
            }
        };
        let child = template.and_then(|t| t.get(&key));
        let json = within_template(lua, &v, child, ancestors).map_err(|e| e.at(&k))?;
        map.insert(key, json);
    }
    for (key, _) in template.into_iter().flatten().filter(|(_, v)| v.is_null()) {
        map.entry(key.as_str()).or_insert(JsonValue::Null);
    }
    Ok(JsonValue::Object(map))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use mlua::{Lua, Value};
    use regex::Regex;
    use serde_json::Value as JsonValue;
    use test_case::test_case;

    use super::{
        MAX_ARRAY_HOLES, MAX_JSON_DEPTH, RECURSIVE_TABLE_ERR, TOO_DEEP_ERR, TableExt,
        UNSUPPORTED_JSON_KEY_ERR, UNSUPPORTED_JSON_VALUE_ERR, json_to_lua, lua_to_json,
        lua_to_json_within,
    };

    const FLAG_KEY: &str = "flag";

    const SRC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
    const TEST_MODULE_ATTR: &str = "#[cfg(test)]";
    /// Table reads that would take `maki.NIL` for a real value: a turbofish
    /// `get::<Option<..>>` or `get::<bool>`, or a `get` into a binding typed
    /// `Option<..>` or `bool`. A `HashMap::get` answers an `Option` with no
    /// `?`, `.ok()` or `.unwrap_or` after it, so it doesn't match.
    const BYPASS_PATTERN: &str = r"\.(?:raw_)?get::<(?:Option<|bool>)|let\s+(?:mut\s+)?\w+\s*:\s*(?:Option<[^=;]*>|bool)\s*=\s*[\w.]+\s*\.(?:raw_)?get\([^()]*\)\s*(?:\?|\.ok\(\)|\.unwrap_or)";
    const BYPASS_MSG: &str =
        "read optional keys with `TableExt::opt` / `opt_bool` so `maki.NIL` counts as absent";

    /// The name `LAYER_CASES` snippets edit through, standing in for the
    /// `value` argument a real hook layer is handed.
    const LAYER_GLOBAL: &str = "value";

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// Blanks comments and top-level `#[cfg(test)] mod` blocks but keeps the
    /// line count, so a match still points at the right line. Tests only read
    /// tables the host built, where `maki.NIL` never shows up.
    fn production_code(source: &str) -> String {
        let mut in_tests = false;
        let mut prev = "";
        source
            .lines()
            .map(|line| {
                in_tests |= prev == TEST_MODULE_ATTR && line.starts_with("mod ");
                let keep = !in_tests && !line.trim_start().starts_with("//");
                in_tests &= line != "}";
                prev = line;
                if keep { line } else { "" }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn optional_reads_go_through_table_ext() {
        let bypass = Regex::new(BYPASS_PATTERN).unwrap();
        let mut files = Vec::new();
        rust_files(Path::new(SRC_DIR), &mut files);
        let offenders: Vec<String> = files
            .iter()
            .flat_map(|path| {
                let code = production_code(&fs::read_to_string(path).unwrap());
                bypass
                    .find_iter(&code)
                    .map(|m| {
                        let line = code[..m.start()].matches('\n').count() + 1;
                        format!("{}:{line}", path.display())
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "{BYPASS_MSG}:\n{}",
            offenders.join("\n")
        );
    }

    #[test_case(None ; "missing_key_is_none")]
    #[test_case(Some(true) ; "explicit_true")]
    #[test_case(Some(false) ; "explicit_false_is_not_absent")]
    fn opt_bool_distinguishes_absent_from_false(value: Option<bool>) {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        if let Some(value) = value {
            tbl.raw_set(FLAG_KEY, value).unwrap();
        }
        assert_eq!(tbl.opt_bool(FLAG_KEY), value);
    }

    #[test]
    fn opt_reads_the_nil_sentinel_as_absent() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set(FLAG_KEY, Value::NULL).unwrap();
        assert_eq!(tbl.opt_bool(FLAG_KEY), None);
        assert_eq!(tbl.opt::<String>(FLAG_KEY).unwrap(), None);
    }

    #[test]
    fn opt_rejects_a_present_value_of_the_wrong_type() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set(FLAG_KEY, lua.create_table().unwrap()).unwrap();
        assert!(tbl.opt::<String>(FLAG_KEY).is_err());
    }

    #[test_case(Value::Nil, JsonValue::Null ; "nil_to_null")]
    #[test_case(Value::NULL, JsonValue::Null ; "sentinel_to_null")]
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

    #[test_case("return { 1, 2, 3 }", "[1,2,3]" ; "dense")]
    #[test_case(r#"return { "a", nil, "c" }"#, r#"["a",null,"c"]"# ; "small_gap")]
    #[test_case("return { [10] = 1 }", "[null,null,null,null,null,null,null,null,null,1]" ; "gaps_up_to_the_safe_length")]
    #[test_case("local t = {} for i = 2, 12, 2 do t[i] = i end return t", "[null,2,null,4,null,6,null,8,null,10,null,12]" ; "half_full_past_the_safe_length")]
    #[test_case("return { [11] = 1 }", r#"{"11":1}"# ; "lone_key_past_the_safe_length")]
    #[test_case(r#"return { [1] = "a", [12] = "b" }"#, r#"{"1":"a","12":"b"}"# ; "excessively_sparse")]
    fn lua_to_json_untagged_integer_keys_follow_cjson(chunk: &str, expected: &str) {
        let lua = Lua::new();
        let value: Value = lua.load(chunk).eval().unwrap();
        let result = lua_to_json(&lua, &value).unwrap();
        assert_eq!(result, serde_json::from_str::<JsonValue>(expected).unwrap());
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

    #[test_case("return { nested = function() end }", UNSUPPORTED_JSON_VALUE_ERR, Some(".nested") ; "function")]
    #[test_case("return { nested = coroutine.create(function() end) }", UNSUPPORTED_JSON_VALUE_ERR, Some(".nested") ; "thread")]
    #[test_case("return { nested = newproxy() }", UNSUPPORTED_JSON_VALUE_ERR, Some(".nested") ; "foreign_userdata")]
    #[test_case("return function() end", UNSUPPORTED_JSON_VALUE_ERR, None ; "top_level_value_has_no_path")]
    #[test_case("return { [{}] = 1 }", UNSUPPORTED_JSON_KEY_ERR, None ; "table_key")]
    #[test_case("return { a = { [{}] = 1 } }", UNSUPPORTED_JSON_KEY_ERR, Some(".a") ; "nested_table_key_names_its_table")]
    #[test_case("local t = {}; t.again = { t }; return t", RECURSIVE_TABLE_ERR, Some(".again[1]") ; "table_that_contains_itself")]
    #[test_case("return { children = { {}, {}, { cb = print } } }", UNSUPPORTED_JSON_VALUE_ERR, Some(".children[3].cb") ; "array_index_then_field")]
    #[test_case(r#"return { ["a.b"] = { [2.5] = print } }"#, UNSUPPORTED_JSON_VALUE_ERR, Some(r#"["a.b"][2.5]"#) ; "keys_dot_cannot_reach")]
    fn lua_to_json_rejects_what_json_cannot_hold(chunk: &str, expected: &str, path: Option<&str>) {
        let lua = Lua::new();
        let value: Value = lua.load(chunk).eval().unwrap();
        let err = lua_to_json(&lua, &value).unwrap_err().to_string();
        assert!(err.contains(expected), "got: {err}");
        assert_eq!(
            err.rsplit_once(" at ").map(|(_, at)| at),
            path,
            "got: {err}"
        );
    }

    #[test]
    fn lua_to_json_accepts_a_table_shared_by_siblings() {
        let lua = Lua::new();
        let value: Value = lua
            .load("local s = { 1 }; return { a = s, b = s }")
            .eval()
            .unwrap();
        let result = lua_to_json(&lua, &value).unwrap();
        assert_eq!(result, serde_json::json!({"a": [1], "b": [1]}));
    }

    #[test_case(MAX_JSON_DEPTH, true ; "at_the_limit")]
    #[test_case(MAX_JSON_DEPTH + 1, false ; "past_the_limit")]
    fn lua_to_json_bounds_nesting(depth: usize, ok: bool) {
        let lua = Lua::new();
        let mut value = Value::Table(lua.create_table().unwrap());
        for _ in 1..depth {
            let outer = lua.create_table().unwrap();
            outer.raw_set(1, value).unwrap();
            value = Value::Table(outer);
        }
        match lua_to_json(&lua, &value) {
            Ok(_) => assert!(ok),
            Err(err) => {
                let err = err.to_string();
                assert!(!ok && err.contains(TOO_DEEP_ERR), "got: {err}");
                assert!(
                    err.ends_with(&format!(" at {}", "[1]".repeat(MAX_JSON_DEPTH))),
                    "got: {err}"
                );
            }
        }
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

    #[test]
    fn consume_conversion_turns_null_into_nil() {
        let lua = Lua::new();
        let value = json_to_lua(&lua, &serde_json::json!({"a": null, "b": [1, null]})).unwrap();
        let table = value.as_table().unwrap();
        assert!(!table.contains_key("a").unwrap());
        let list: mlua::Table = table.get("b").unwrap();
        assert_eq!(list.raw_len(), 1);
        assert!(json_to_lua(&lua, &JsonValue::Null).unwrap().is_nil());
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
        (r#"{"a":[1,2]}"#, "value.a = {}", r#"{"a":[]}"#),
        (r#"{"a":{"b":1}}"#, "value.a = {}", r#"{"a":{}}"#),
    ];

    #[test_case(0 ; "truncating_a_real_value_shortens_the_array")]
    #[test_case(1 ; "a_null_replaced_by_a_value_keeps_the_value")]
    #[test_case(2 ; "an_object_replaced_by_a_scalar_ends_the_template")]
    #[test_case(3 ; "deleting_a_non_null_key_still_deletes_it")]
    #[test_case(4 ; "a_fresh_empty_table_in_place_of_an_array_stays_an_array")]
    #[test_case(5 ; "a_fresh_empty_table_in_place_of_an_object_stays_an_object")]
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
