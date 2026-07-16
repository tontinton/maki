use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Table, Value as LuaValue};

use crate::language::Language;

struct LangRegistry {
    lang_to_filetypes: HashMap<String, Vec<String>>,
    filetype_to_lang: HashMap<String, String>,
}

/// Registers {lang} for use with tree-sitter.
/// Call this to confirm a language grammar is available. Throws if {lang} is unknown.
/// Custom grammar paths are not yet supported.
///
/// @param lang string Language name, e.g. `"rust"`.
/// @param opts table? Options table (the `path` key is not yet supported).
/// @example
/// maki.treesitter.language.add("lua")
#[lua_fn]
fn add(_lua: &Lua, lang: String, opts: Option<Table>) -> mlua::Result<()> {
    if let Some(ref opts) = opts
        && opts.contains_key("path")?
    {
        return Err(mlua::Error::runtime(
            "custom grammar paths not supported yet",
        ));
    }
    if Language::from_name(&lang).is_none() {
        return Err(mlua::Error::runtime(format!("language not found: {lang}")));
    }
    Ok(())
}

/// Associates {lang} with one or more filetypes, so you can look up the right
/// parser language for a given filetype later with `get_lang()`.
///
/// @param lang string Language name.
/// @param filetype string|table A single filetype string or an array of filetype strings.
/// @example
/// maki.treesitter.language.register("typescript", { "ts", "tsx" })
#[lua_fn]
fn register(
    _lua: &Lua,
    #[ctx] reg: Arc<Mutex<LangRegistry>>,
    lang: String,
    filetype: LuaValue,
) -> mlua::Result<()> {
    let filetypes: Vec<String> = match filetype {
        LuaValue::String(s) => vec![s.to_str()?.to_owned()],
        LuaValue::Table(tbl) => {
            let mut v = Vec::new();
            for pair in tbl.sequence_values::<String>() {
                v.push(pair?);
            }
            v
        }
        other => {
            return Err(mlua::Error::runtime(format!(
                "register: expected string or table, got {}",
                other.type_name()
            )));
        }
    };
    let mut reg = reg
        .lock()
        .map_err(|_| mlua::Error::runtime("language registry lock poisoned"))?;
    for ft in &filetypes {
        reg.filetype_to_lang.insert(ft.clone(), lang.clone());
    }
    let existing = reg.lang_to_filetypes.entry(lang.clone()).or_default();
    for ft in filetypes {
        if !existing.contains(&ft) {
            existing.push(ft);
        }
    }
    Ok(())
}

/// Looks up the tree-sitter language name for {filetype}.
/// Returns the registered language, or falls back to {filetype} itself if
/// a grammar with that name exists. Returns nil when nothing matches.
///
/// @param filetype string Filetype to look up, e.g. `"ts"`.
/// @return (string|nil) Language name, or nil.
/// @example
/// local lang = maki.treesitter.language.get_lang("tsx")
/// if lang then print(lang) end -- "typescript"
#[lua_fn]
fn get_lang(
    _lua: &Lua,
    #[ctx] reg: Arc<Mutex<LangRegistry>>,
    filetype: String,
) -> mlua::Result<Option<String>> {
    let guard = reg
        .lock()
        .map_err(|_| mlua::Error::runtime("language registry lock poisoned"))?;
    if let Some(lang) = guard.filetype_to_lang.get(&filetype) {
        return Ok(Some(lang.clone()));
    }
    drop(guard);
    if Language::from_name(&filetype).is_some() {
        return Ok(Some(filetype));
    }
    Ok(None)
}

/// Returns all filetypes that have been registered for {lang}.
///
/// @param lang string Language name.
/// @return (table) Array of filetype strings.
/// @example
/// local fts = maki.treesitter.language.get_filetypes("typescript")
/// -- { "ts", "tsx" }
#[lua_fn]
fn get_filetypes(
    lua: &Lua,
    #[ctx] reg: Arc<Mutex<LangRegistry>>,
    lang: String,
) -> mlua::Result<Table> {
    let guard = reg
        .lock()
        .map_err(|_| mlua::Error::runtime("language registry lock poisoned"))?;
    let tbl = lua.create_table()?;
    if let Some(fts) = guard.lang_to_filetypes.get(&lang) {
        for (i, ft) in fts.iter().enumerate() {
            tbl.raw_set(i + 1, ft.as_str())?;
        }
    }
    Ok(tbl)
}

/// Returns metadata about the grammar for {lang}.
/// Useful for debugging or discovering which node types and fields a grammar defines.
///
/// @param lang string Language name.
/// @return (table) Table with keys `abi_version` (integer), `node_types` (string[]), `fields` (string[]).
/// @example
/// local info = maki.treesitter.language.inspect("lua")
/// print("ABI: " .. info.abi_version)
/// for _, nt in ipairs(info.node_types) do print(nt) end
#[lua_fn]
fn inspect(lua: &Lua, lang: String) -> mlua::Result<Table> {
    let language = Language::from_name(&lang)
        .ok_or_else(|| mlua::Error::runtime(format!("language not found: {lang}")))?
        .ts_language();

    let tbl = lua.create_table()?;
    tbl.raw_set("abi_version", language.abi_version())?;

    let node_types = lua.create_table()?;
    let count = language.node_kind_count();
    let mut idx = 1usize;
    for id in 0..count as u16 {
        if let Some(name) = language.node_kind_for_id(id) {
            node_types.raw_set(idx, name)?;
            idx += 1;
        }
    }
    tbl.raw_set("node_types", node_types)?;

    let fields = lua.create_table()?;
    let field_count = language.field_count();
    let mut fidx = 1usize;
    for id in 1..=field_count as u16 {
        if let Some(name) = language.field_name_for_id(id) {
            fields.raw_set(fidx, name)?;
            fidx += 1;
        }
    }
    tbl.raw_set("fields", fields)?;

    Ok(tbl)
}

lua_table! {
    /// Language registry for tree-sitter grammars.
    ///
    /// Mirrors `vim.treesitter.language`. Use these functions to register grammars,
    /// map filetypes to languages, and inspect available node types.
    ///
    /// ```lua
    /// maki.treesitter.language.add("lua")
    /// maki.treesitter.language.register("lua", "luau")
    /// ```
    extend "maki.treesitter.language" => fn language_fns(reg: Arc<Mutex<LangRegistry>>), DOCS [
        add, register(reg), get_lang(reg), get_filetypes(reg), inspect,
    ]
}

pub(crate) fn create_language_module(lua: &Lua) -> mlua::Result<Table> {
    let reg = Arc::new(Mutex::new(LangRegistry {
        lang_to_filetypes: HashMap::new(),
        filetype_to_lang: HashMap::new(),
    }));
    let t = lua.create_table()?;
    language_fns(&t, lua, reg)?;
    Ok(t)
}
