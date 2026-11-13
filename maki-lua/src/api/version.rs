//! `maki.version`: the `vim.version` API on top of the `semver` crate, so
//! parsing and precedence follow SemVer.

use std::cmp::Ordering;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, MetaMethod, Result as LuaResult, Table, Value};
use semver::{BuildMetadata, Prerelease, Version};

use crate::api::util::convert::{NilAsNone, TableExt};
use crate::api::util::pair::{Pair, err_pair};
use crate::plugin_permissions::RUNTIME_VERSION;

const VERSION_METATABLE: &str = "maki.version.Version";
const COMPONENTS: [&str; 3] = ["major", "minor", "patch"];
const INVALID_VERSION_ERR: &str = "invalid version";

fn component_name(key: &Value) -> Option<&'static str> {
    let index = match *key {
        Value::Integer(index) => index as f64,
        Value::Number(index) => index,
        _ => return None,
    };
    if index.fract() != 0.0 || !(1.0..=3.0).contains(&index) {
        return None;
    }
    COMPONENTS.get(index as usize - 1).copied()
}

fn version_from_table(table: &Table) -> LuaResult<Version> {
    if table.raw_get::<Value>("major")?.is_nil() {
        let tuple = |index: usize| -> LuaResult<u64> {
            Ok(table
                .raw_get::<NilAsNone<u64>>(index)?
                .0
                .unwrap_or_default())
        };
        return Ok(Version::new(tuple(1)?, tuple(2)?, tuple(3)?));
    }
    let invalid =
        |field: &str| mlua::Error::runtime(format!("{INVALID_VERSION_ERR}: bad `{field}`"));
    let number = |field: &str| table.raw_get::<u64>(field).map_err(|_| invalid(field));
    let suffix =
        |field: &str| -> LuaResult<String> { Ok(table.opt::<String>(field)?.unwrap_or_default()) };
    let mut version = Version::new(number("major")?, number("minor")?, number("patch")?);
    version.pre = Prerelease::new(&suffix("prerelease")?).map_err(|_| invalid("prerelease"))?;
    version.build = BuildMetadata::new(&suffix("build")?).map_err(|_| invalid("build"))?;
    Ok(version)
}

/// Neovim's loose parse: the version starts at the first digit and ends at
/// whitespace. Missing minor and patch are 0, and anything after the numbers
/// is dropped unless it is a valid `-prerelease` or `+build` suffix.
fn coerce(text: &str) -> Option<Version> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let word = text[start..].split_whitespace().next()?;
    let core_len = word
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(word.len());
    let (core, suffix) = word.split_at(core_len);
    let mut parts = core.split('.').map(|part| match part {
        "" => Ok(0),
        _ => part.parse::<u64>(),
    });
    let mut component = || parts.next().unwrap_or(Ok(0)).ok();
    let (major, minor, patch) = (component()?, component()?, component()?);
    Some(
        Version::parse(&format!("{major}.{minor}.{patch}{suffix}"))
            .unwrap_or_else(|_| Version::new(major, minor, patch)),
    )
}

/// Reads a version string, a version table, or a `{major, minor, patch}`
/// tuple. `None` means a string that does not parse. Other types are
/// programmer errors and raise.
fn to_version(value: &Value, strict: bool) -> LuaResult<Option<Version>> {
    match value {
        Value::String(text) => Ok(text.to_str().ok().and_then(|text| match strict {
            true => Version::parse(&text).ok(),
            false => coerce(&text),
        })),
        Value::Table(table) => version_from_table(table).map(Some),
        other => Err(mlua::Error::runtime(format!(
            "{INVALID_VERSION_ERR}: expected string or table, got {}",
            other.type_name()
        ))),
    }
}

fn invalid_version(value: &Value) -> String {
    match value {
        Value::String(text) => format!("{INVALID_VERSION_ERR}: \"{}\"", text.display()),
        _ => INVALID_VERSION_ERR.to_owned(),
    }
}

fn comparison(lua: &Lua, test: fn(Ordering) -> bool) -> LuaResult<mlua::Function> {
    lua.create_function(move |_, (left, right): (Table, Table)| {
        Ok(test(
            version_from_table(&left)?.cmp_precedence(&version_from_table(&right)?),
        ))
    })
}

/// Every version table shares one metatable: Luau only calls `__eq` and
/// `__lt` when both operands carry the same metamethod. It is read-only, so
/// no plugin can swap a metamethod that runs inside another plugin's compare.
fn version_metatable(lua: &Lua) -> LuaResult<Table> {
    if let Some(metatable) = lua.named_registry_value::<Option<Table>>(VERSION_METATABLE)? {
        return Ok(metatable);
    }
    let metatable = lua.create_table()?;
    metatable.set(
        MetaMethod::Index.name(),
        lua.create_function(
            |_, (this, key): (Table, Value)| match component_name(&key) {
                Some(name) => this.raw_get::<Value>(name),
                None => Ok(Value::Nil),
            },
        )?,
    )?;
    metatable.set(
        MetaMethod::NewIndex.name(),
        lua.create_function(|_, (this, key, value): (Table, Value, Value)| {
            match component_name(&key) {
                Some(name) => this.raw_set(name, value),
                None => this.raw_set(key, value),
            }
        })?,
    )?;
    metatable.set(MetaMethod::Eq.name(), comparison(lua, Ordering::is_eq)?)?;
    metatable.set(MetaMethod::Lt.name(), comparison(lua, Ordering::is_lt)?)?;
    metatable.set(MetaMethod::Le.name(), comparison(lua, Ordering::is_le)?)?;
    metatable.set(
        MetaMethod::ToString.name(),
        lua.create_function(|_, this: Table| Ok(version_from_table(&this)?.to_string()))?,
    )?;
    metatable.set_readonly(true);
    lua.set_named_registry_value(VERSION_METATABLE, &metatable)?;
    Ok(metatable)
}

fn version_table(lua: &Lua, version: &Version) -> LuaResult<Table> {
    let table = lua.create_table()?;
    table.raw_set("major", version.major)?;
    table.raw_set("minor", version.minor)?;
    table.raw_set("patch", version.patch)?;
    table.raw_set(
        "prerelease",
        (!version.pre.is_empty()).then(|| version.pre.as_str()),
    )?;
    table.raw_set(
        "build",
        (!version.build.is_empty()).then(|| version.build.as_str()),
    )?;
    table.set_metatable(Some(version_metatable(lua)?))?;
    Ok(table)
}

fn compare_values(left: &Value, right: &Value) -> LuaResult<Pair<Ordering>> {
    let Some(left_version) = to_version(left, false)? else {
        return Ok(err_pair(invalid_version(left)));
    };
    let Some(right_version) = to_version(right, false)? else {
        return Ok(err_pair(invalid_version(right)));
    };
    Ok((Some(left_version.cmp_precedence(&right_version)), None))
}

fn test_values(left: &Value, right: &Value, test: fn(Ordering) -> bool) -> LuaResult<Pair<bool>> {
    let (ordering, err) = compare_values(left, right)?;
    Ok((ordering.map(test), err))
}

/// Parse a version string, such as "1.2.3-rc.1+build.5", into a version
/// table with `major`, `minor`, `patch`, and optional `prerelease` and `build`.
/// `[1]`, `[2]` and `[3]` alias the numbers. Two version tables compare with
/// `==`, `<`, `<=`, `>` and `>=`, and `tostring` prints one.
///
/// Parsing is loose by default, like Neovim, so "v1.2", "tmux 3.2a" and
/// "git version 2.43.0" all parse. The version starts at the first digit and
/// ends at whitespace. Missing minor and patch are 0. Trailing text is dropped
/// unless it is a valid SemVer `-prerelease` or `+build` suffix.
///
/// @param version string Version string to parse.
/// @param opts table? Options:
///   `strict` (boolean) only accept a full SemVer 2.0.0 string. Off by default.
/// @return (table?, string?) Version table.
/// @example
/// local v = maki.version.parse("1.0.1-rc1+build.2")
/// print(v.major, v.prerelease, tostring(v)) -- 1  rc1  1.0.1-rc1+build.2
/// maki.version.parse("git version 2.43.0") -- 2.43.0
/// maki.version.parse("1.0", { strict = true }) -- nil, invalid version: "1.0"
#[lua_fn]
fn parse(lua: &Lua, version: Value, opts: Option<Table>) -> LuaResult<Pair<Table>> {
    let Value::String(_) = &version else {
        return Err(mlua::Error::runtime(format!(
            "{INVALID_VERSION_ERR}: expected string, got {}",
            version.type_name()
        )));
    };
    let strict = opts.as_ref().opt_bool("strict").unwrap_or_default();
    match to_version(&version, strict)? {
        Some(parsed) => Ok((Some(version_table(lua, &parsed)?), None)),
        None => Ok(err_pair(invalid_version(&version))),
    }
}

/// Compare two versions by SemVer precedence. Build metadata is ignored, and
/// a prerelease sorts before its release.
///
/// @param v1 table|string First version.
/// @param v2 table|string Second version.
/// @return (integer?, string?) -1, 0 or 1 when `v1` is lower, equal or higher.
/// @example
/// maki.version.cmp({1, 0, 3}, "0.2.1") -- 1, nil
/// maki.version.cmp("1.2.3-rc.2", "1.2.3-rc.10") -- -1, nil
/// maki.version.cmp("v1.2", "1.2.0") -- 0, nil
#[lua_fn]
fn cmp(_lua: &Lua, v1: Value, v2: Value) -> LuaResult<Pair<i8>> {
    let (ordering, err) = compare_values(&v1, &v2)?;
    Ok((ordering.map(|ordering| ordering as i8), err))
}

/// Whether `v1 == v2`, by `maki.version.cmp` rules.
///
/// @param v1 table|string First version.
/// @param v2 table|string Second version.
/// @return (boolean?, string?) The result.
#[lua_fn]
fn eq(_lua: &Lua, v1: Value, v2: Value) -> LuaResult<Pair<bool>> {
    test_values(&v1, &v2, Ordering::is_eq)
}

/// Whether `v1 < v2`, by `maki.version.cmp` rules.
///
/// @param v1 table|string First version.
/// @param v2 table|string Second version.
/// @return (boolean?, string?) The result.
#[lua_fn]
fn lt(_lua: &Lua, v1: Value, v2: Value) -> LuaResult<Pair<bool>> {
    test_values(&v1, &v2, Ordering::is_lt)
}

/// Whether `v1 <= v2`, by `maki.version.cmp` rules.
///
/// @param v1 table|string First version.
/// @param v2 table|string Second version.
/// @return (boolean?, string?) The result.
#[lua_fn]
fn le(_lua: &Lua, v1: Value, v2: Value) -> LuaResult<Pair<bool>> {
    test_values(&v1, &v2, Ordering::is_le)
}

/// Whether `v1 > v2`, by `maki.version.cmp` rules.
///
/// @param v1 table|string First version.
/// @param v2 table|string Second version.
/// @return (boolean?, string?) The result.
#[lua_fn]
fn gt(_lua: &Lua, v1: Value, v2: Value) -> LuaResult<Pair<bool>> {
    test_values(&v1, &v2, Ordering::is_gt)
}

/// Whether `v1 >= v2`, by `maki.version.cmp` rules.
///
/// @param v1 table|string First version.
/// @param v2 table|string Second version.
/// @return (boolean?, string?) The result.
#[lua_fn]
fn ge(_lua: &Lua, v1: Value, v2: Value) -> LuaResult<Pair<bool>> {
    test_values(&v1, &v2, Ordering::is_ge)
}

/// Return the highest version in a list, as the element itself.
///
/// @param versions table List of versions.
/// @return (any, string?) The highest element, or nil for an empty list.
/// @example
/// maki.version.last({ "1.9.0", "1.10.0", "1.2.0" }) -- "1.10.0", nil
#[lua_fn]
fn last(_lua: &Lua, versions: Table) -> LuaResult<Pair<Value>> {
    let mut highest: Option<(Value, Version)> = None;
    for value in versions.sequence_values::<Value>() {
        let value = value?;
        let Some(version) = to_version(&value, false)? else {
            return Ok(err_pair(invalid_version(&value)));
        };
        if highest
            .as_ref()
            .is_none_or(|(_, best)| version.cmp_precedence(best).is_gt())
        {
            highest = Some((value, version));
        }
    }
    Ok((highest.map(|(value, _)| value), None))
}

lua_table! {
    /// Version parsing and comparison with the same API as Neovim's
    /// `vim.version`. `maki.version()` returns the running maki version as a
    /// version table.
    ///
    /// Every function except `parse` takes a version table, a
    /// `{major, minor, patch}` tuple or a string, parsed loosely like
    /// `maki.version.parse`. A string that does not parse returns
    /// `(nil, err)`. Any other type raises a Lua error.
    ///
    /// ```lua
    /// if maki.version.ge(maki.version(), "0.7.0") then ... end
    /// ```
    "maki.version" => fn create_version_functions(), DOCS [
        parse, cmp, eq, lt, le, gt, ge, last,
    ]
}

pub(crate) fn create_version_table(lua: &Lua) -> LuaResult<Table> {
    let table = create_version_functions(lua)?;
    let metatable = lua.create_table()?;
    metatable.set(
        MetaMethod::Call.name(),
        lua.create_function(|lua, _module: Value| {
            let running = Version::parse(RUNTIME_VERSION).map_err(mlua::Error::external)?;
            version_table(lua, &running)
        })?,
    )?;
    table.set_metatable(Some(metatable))?;
    Ok(table)
}

#[cfg(test)]
mod tests {
    use mlua::Lua;
    use test_case::test_case;

    use super::{INVALID_VERSION_ERR, RUNTIME_VERSION, create_version_table};

    fn eval(lua: &Lua, expr: &str) -> String {
        lua.globals()
            .set("V", create_version_table(lua).unwrap())
            .unwrap();
        lua.load(format!(
            "local value, err = {expr}
             return value == nil and ('nil|' .. tostring(err)) or tostring(value)"
        ))
        .eval()
        .unwrap()
    }

    fn eval_err(expr: &str) -> String {
        let lua = Lua::new();
        lua.globals()
            .set("V", create_version_table(&lua).unwrap())
            .unwrap();
        lua.load(format!("return {expr}"))
            .exec()
            .unwrap_err()
            .to_string()
    }

    #[test_case("1.2.3", false, "1.2.3" ; "loose_release")]
    #[test_case("1.2.3-rc.1+b.5", false, "1.2.3-rc.1+b.5" ; "loose_prerelease_and_build")]
    #[test_case("git version 2.43.0", false, "2.43.0" ; "loose_surrounding_text")]
    #[test_case("v1.2", false, "1.2.0" ; "loose_leading_v_partial")]
    #[test_case("1", false, "1.0.0" ; "loose_major_only")]
    #[test_case("01.2.3", false, "1.2.3" ; "loose_leading_zero")]
    #[test_case("tmux 3.2a", false, "3.2.0" ; "loose_trailing_letter")]
    #[test_case("nvim 0.10.0-dev-2945+g3e9b2d3 (x)", false, "0.10.0-dev-2945+g3e9b2d3" ; "loose_suffix_then_text")]
    #[test_case("1.2.3.4", false, "1.2.3" ; "loose_extra_component")]
    #[test_case("1.2.3-a!b", false, "1.2.3" ; "loose_bad_prerelease_dropped")]
    #[test_case("gcc (Debian 12.2.0-14) 12.2.0", false, "12.2.0" ; "loose_bad_suffix_dropped")]
    #[test_case("1.2.3", true, "1.2.3" ; "strict_release")]
    #[test_case("1.2.3-rc.1+b.5", true, "1.2.3-rc.1+b.5" ; "strict_prerelease_and_build")]
    #[test_case("1.2.3+build", true, "1.2.3+build" ; "strict_build_only")]
    fn parse_accepts(input: &str, strict: bool, expected: &str) {
        let expr = format!("V.parse({input:?}, {{ strict = {strict} }})");
        assert_eq!(eval(&Lua::new(), &expr), expected);
    }

    #[test_case("garbage", false ; "loose_garbage")]
    #[test_case("", false ; "loose_empty")]
    #[test_case("99999999999999999999.0.0", false ; "loose_overflow")]
    #[test_case("garbage", true ; "strict_garbage")]
    #[test_case("1", true ; "strict_major_only")]
    #[test_case("1.2", true ; "strict_partial")]
    #[test_case("v1.2.3", true ; "strict_leading_v")]
    #[test_case("01.2.3", true ; "strict_leading_zero")]
    #[test_case("1.2.3-a!b", true ; "strict_bad_prerelease")]
    #[test_case("git version 2.43.0", true ; "strict_surrounding_text")]
    fn parse_rejects(input: &str, strict: bool) {
        let expr = format!("V.parse({input:?}, {{ strict = {strict} }})");
        assert_eq!(
            eval(&Lua::new(), &expr),
            format!("nil|{INVALID_VERSION_ERR}: \"{input}\"")
        );
    }

    #[test]
    fn parse_is_loose_without_opts() {
        assert_eq!(eval(&Lua::new(), "V.parse('v1.2')"), "1.2.0");
    }

    #[test_case("V.cmp('1.2.3-rc.2', '1.2.3-rc.10')", "-1" ; "numeric_identifier")]
    #[test_case("V.cmp('1.2.3-alpha', '1.2.3-alpha.1')", "-1" ; "shorter_prerelease_first")]
    #[test_case("V.cmp('1.2.3', '1.2.3-rc1')", "1" ; "release_after_prerelease")]
    #[test_case("V.eq('1.2.3+a', '1.2.3+b')", "true" ; "build_ignored")]
    #[test_case("V.gt({1, 0, 3}, {0, 2, 1})", "true" ; "tuples")]
    #[test_case("V.cmp({1}, '1.0.0')", "0" ; "short_tuple")]
    #[test_case("V.lt({ major = 1, minor = 2, patch = 3, prerelease = 'rc' }, '1.2.3')", "true" ; "version_shaped_table")]
    #[test_case("V.parse('1.2.3') < V.parse('1.10.0')", "true" ; "lt_metamethod")]
    #[test_case("V.parse('1.2.3') <= V.parse('1.2.3+x')", "true" ; "le_metamethod")]
    #[test_case("(function() local v = V.parse('1.2.3'); v[2] = 9; return v end)()", "1.9.3" ; "index_aliases")]
    #[test_case("pcall(function() getmetatable(V.parse('1.0.0')).__lt = nil end)", "false" ; "metatable_read_only")]
    #[test_case("V.last({ '1.9.0', '1.10.0', '1.2.0' })", "1.10.0" ; "last_by_version")]
    #[test_case("V.last({})", "nil|nil" ; "last_empty")]
    #[test_case("V.cmp('v1.2', '1.2.0')", "0" ; "loose_strings")]
    #[test_case("V.ge('git version 2.43.0', '2.30')", "true" ; "loose_tool_output")]
    fn compare(expr: &str, expected: &str) {
        assert_eq!(eval(&Lua::new(), expr), expected);
    }

    #[test]
    fn compare_answers_err_for_garbage() {
        assert_eq!(
            eval(&Lua::new(), "V.cmp('garbage', '1.0.0')"),
            format!("nil|{INVALID_VERSION_ERR}: \"garbage\"")
        );
    }

    #[test_case("V.cmp(5, '1.0.0')" ; "cmp_number")]
    #[test_case("V.parse({1, 2, 3})" ; "parse_table")]
    #[test_case("V.lt({ major = 'x' }, '1.0.0')" ; "malformed_table")]
    #[test_case("V.lt({ major = 1, minor = 0, patch = 0, prerelease = 'a!' }, '1.0.0')" ; "bad_prerelease_field")]
    fn wrong_types_throw(expr: &str) {
        assert!(eval_err(expr).contains(INVALID_VERSION_ERR));
    }

    #[test]
    fn call_returns_running_version() {
        assert_eq!(eval(&Lua::new(), "V()"), RUNTIME_VERSION);
    }

    /// Each plugin gets its own `maki.version` table, but versions from all of
    /// them share one VM and must still compare with `==`.
    #[test]
    fn versions_from_separate_tables_compare() {
        let lua = Lua::new();
        lua.globals()
            .set("Other", create_version_table(&lua).unwrap())
            .unwrap();
        assert_eq!(
            eval(&lua, "V.parse('1.2.3') == Other.parse('1.2.3+b')"),
            "true"
        );
    }
}
