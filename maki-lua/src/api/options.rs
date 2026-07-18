use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use maki_lua_macro::lua_fn;
use mlua::{Lua, Result as LuaResult, Table, Value as LuaValue};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::api::util::convert::json_to_lua;

/// The user's `plugins.<name>` table from `maki.setup`, passed through opaquely.
pub(crate) type PluginOpts = Arc<JsonMap<String, JsonValue>>;

/// Option specs declared via `register_options`, keyed by plugin name. Read
/// by docgen and by the loader to reject `plugins.<name>` keys no plugin ever
/// declared.
pub type PluginOptionSpecs = BTreeMap<Arc<str>, Vec<OptionSpec>>;

const SPEC_KEYS: &[&str] = &["default", "type", "min", "desc"];
/// `plugins.<name>.enabled` is consumed by the config layer and never reaches
/// the plugin, so declaring it would be dead.
const RESERVED_NAMES: &[&str] = &["enabled"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionType {
    Boolean,
    Integer,
    Number,
    String,
}

impl OptionType {
    const ALL: &[Self] = &[Self::Boolean, Self::Integer, Self::Number, Self::String];

    fn as_str(self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::String => "string",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == s)
    }

    fn valid_list() -> String {
        Self::ALL
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn of_json(value: &JsonValue) -> Option<Self> {
        match value {
            JsonValue::Bool(_) => Some(Self::Boolean),
            JsonValue::Number(n) if n.is_i64() || n.is_u64() => Some(Self::Integer),
            JsonValue::Number(_) => Some(Self::Number),
            JsonValue::String(_) => Some(Self::String),
            _ => None,
        }
    }

    fn matches(self, value: &JsonValue) -> bool {
        match self {
            Self::Number => value.is_number(),
            _ => Self::of_json(value) == Some(self),
        }
    }

    fn is_numeric(self) -> bool {
        matches!(self, Self::Integer | Self::Number)
    }
}

impl fmt::Display for OptionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Single source for user-value validation and the generated config docs.
#[derive(Debug, Clone)]
pub struct OptionSpec {
    pub name: String,
    pub ty: OptionType,
    pub default: Option<JsonValue>,
    pub min: Option<f64>,
    pub desc: String,
}

pub(crate) fn collect_plugin_options(lua: &Lua) -> PluginOptionSpecs {
    lua.app_data_ref::<PluginOptionSpecs>()
        .map(|store| store.clone())
        .unwrap_or_default()
}

fn spec_error(name: &str, msg: &str) -> mlua::Error {
    mlua::Error::runtime(format!("register_options: option \"{name}\": {msg}"))
}

fn default_to_json(name: &str, value: LuaValue) -> LuaResult<Option<JsonValue>> {
    Ok(Some(match value {
        LuaValue::Nil => return Ok(None),
        LuaValue::Boolean(b) => JsonValue::Bool(b),
        LuaValue::Integer(i) => JsonValue::from(i),
        LuaValue::Number(n) => serde_json::Number::from_f64(n)
            .map(JsonValue::Number)
            .ok_or_else(|| spec_error(name, "default must be a finite number"))?,
        LuaValue::String(s) => JsonValue::String(s.to_str()?.to_owned()),
        _ => {
            return Err(spec_error(
                name,
                "default must be a boolean, number, or string",
            ));
        }
    }))
}

fn parse_spec(name: &str, spec: &Table) -> LuaResult<OptionSpec> {
    if RESERVED_NAMES.contains(&name) {
        return Err(spec_error(
            name,
            "reserved name (`enabled` is handled by the config layer)",
        ));
    }
    for pair in spec.pairs::<String, LuaValue>() {
        let (key, _) = pair.map_err(|_| spec_error(name, "spec keys must be strings"))?;
        if !SPEC_KEYS.contains(&key.as_str()) {
            return Err(spec_error(
                name,
                &format!(
                    "unknown spec key \"{key}\" (expected one of: {})",
                    SPEC_KEYS.join(", ")
                ),
            ));
        }
    }

    let default = default_to_json(name, spec.get("default")?)?;
    let ty = match spec.get::<Option<String>>("type")? {
        Some(s) => OptionType::parse(&s).ok_or_else(|| {
            spec_error(
                name,
                &format!(
                    "invalid type \"{s}\" (expected one of: {})",
                    OptionType::valid_list()
                ),
            )
        })?,
        None => match &default {
            Some(d) => OptionType::of_json(d).expect("default_to_json only keeps scalar types"),
            None => {
                return Err(spec_error(
                    name,
                    "type is required when there is no default",
                ));
            }
        },
    };
    if let Some(d) = &default
        && !ty.matches(d)
    {
        return Err(spec_error(
            name,
            &format!("default {d} does not match type {ty}"),
        ));
    }

    let min: Option<f64> = spec.get("min")?;
    if let Some(min) = min {
        if !ty.is_numeric() {
            return Err(spec_error(
                name,
                &format!("min is not allowed for type {ty}"),
            ));
        }
        if let Some(d) = &default
            && d.as_f64().is_some_and(|v| v < min)
        {
            return Err(spec_error(
                name,
                &format!("default {d} is below min ({min})"),
            ));
        }
    }

    let desc: String = spec
        .get::<Option<String>>("desc")?
        .filter(|d| !d.is_empty())
        .ok_or_else(|| spec_error(name, "desc is required"))?;

    Ok(OptionSpec {
        name: name.to_owned(),
        ty,
        default,
        min,
        desc,
    })
}

fn validate_user_opts(
    plugin: &str,
    opts: &JsonMap<String, JsonValue>,
    specs: &[OptionSpec],
) -> LuaResult<()> {
    for (key, value) in opts {
        let Some(spec) = specs.iter().find(|s| &s.name == key) else {
            let valid: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
            return Err(mlua::Error::runtime(format!(
                "unknown option \"{key}\" for plugins.{plugin} (valid options: {})",
                valid.join(", ")
            )));
        };
        if !spec.ty.matches(value) {
            return Err(mlua::Error::runtime(format!(
                "invalid value for plugins.{plugin}.{key}: expected {}, got {value}",
                spec.ty
            )));
        }
        if let Some(min) = spec.min
            && value.as_f64().is_some_and(|v| v < min)
        {
            return Err(mlua::Error::runtime(format!(
                "invalid value for plugins.{plugin}.{key}: {value} is below minimum ({min})"
            )));
        }
    }
    Ok(())
}

/// Declare the options your plugin accepts under `plugins.<name>` in
/// `maki.setup`, and get back what the user set merged with your defaults.
/// Call it once, at the top level of your plugin file.
///
/// An unknown key, a wrong type, or a value below `min` fails the plugin
/// load with a clear message, so users catch typos right away. Bad specs
/// fail the load too. The specs also feed the generated configuration docs.
///
/// @param spec table Map of option name to a spec table:
///   default (boolean|number|string) Optional. Used when the user sets nothing. Its Lua type becomes the option type.
///   type    (string) Required when there is no default: "boolean", "integer", "number", or "string".
///   min     (number) Optional. Minimum accepted value, numeric options only.
///   desc    (string) Required. One line shown in the configuration docs.
/// @return (table) Merged options: the user's value where set, otherwise the default, or nil when neither exists.
/// @example
/// local opts = maki.api.register_options({
///   timeout_secs = { default = 120, min = 5, desc = "Kill the command after this many seconds." },
///   max_output_lines = { type = "integer", desc = "Override agent.max_output_lines for this tool." },
/// })
#[lua_fn]
fn register_options(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] opts: PluginOpts,
    spec: Table,
) -> LuaResult<Table> {
    let mut entries: Vec<(String, Table)> = spec
        .pairs::<String, Table>()
        .collect::<LuaResult<_>>()
        .map_err(|e| mlua::Error::runtime(format!("register_options: invalid spec: {e}")))?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let specs: Vec<OptionSpec> = entries
        .iter()
        .map(|(name, s)| parse_spec(name, s))
        .collect::<LuaResult<_>>()?;

    validate_user_opts(&plugin, &opts, &specs)?;

    let merged = lua.create_table()?;
    for spec in &specs {
        if let Some(v) = opts.get(&spec.name).or(spec.default.as_ref()) {
            merged.set(spec.name.as_str(), json_to_lua(lua, v)?)?;
        }
    }

    if let Some(mut store) = lua.app_data_mut::<PluginOptionSpecs>()
        && store.insert(Arc::clone(&plugin), specs).is_some()
    {
        return Err(mlua::Error::runtime(
            "register_options: called more than once; call it once at the top level of the plugin",
        ));
    }
    Ok(merged)
}
