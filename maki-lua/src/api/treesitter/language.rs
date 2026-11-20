use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::mem;
use std::sync::{Arc, LazyLock, Mutex};

use libloading::{Library, Symbol};
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Table, Value as LuaValue};
use tree_sitter::{LANGUAGE_VERSION, Language as TsLanguage, MIN_COMPATIBLE_LANGUAGE_VERSION};
use tree_sitter_language::LanguageFn;

use crate::api::fs::expand_tilde;
use crate::api::util::convert::TableExt;
use crate::api::util::pair::{Pair, err_pair, try_pair};
use crate::language::Language;
use crate::plugin_permissions::{Permission, PluginPermissions, require};

const SYMBOL_PREFIX: &str = "tree_sitter_";
const LANGUAGE_NOT_FOUND: &str = "language not found";
const LOAD_FAILED: &str = "cannot load";

type LanguageConstructor = unsafe extern "C" fn() -> *const ();

/// Grammars `add` loaded from a path. Process wide, like the libraries behind them.
static LOADED_GRAMMARS: LazyLock<Mutex<HashMap<String, TsLanguage>>> =
    LazyLock::new(Mutex::default);

struct LangRegistry {
    lang_to_filetypes: HashMap<String, Vec<String>>,
    filetype_to_lang: HashMap<String, String>,
}

/// Registers {lang} for use with tree-sitter. Like `vim.treesitter.language.add`.
/// Without a `path` it confirms a built-in grammar is available. With one it
/// loads a compiled parser, such as one nvim-treesitter or Helix installed.
/// That runs native code, so it needs the `run` permission.
///
/// @param lang string Language name, e.g. `"rust"`.
/// @param opts table? Options table:
///   - path (string): parser library to load, e.g. `"~/.local/share/nvim/site/parser/nu.so"`.
///   - symbol_name (string): constructor to call. Default `tree_sitter_{lang}`.
/// @return (boolean?, string?) `true`, or nil plus an error if {lang} is unknown or fails to load.
/// @example
/// if not maki.treesitter.language.add("lua") then return end
#[lua_fn]
fn add(
    lua: &Lua,
    #[ctx] grants: PluginPermissions,
    lang: String,
    opts: Option<Table>,
) -> mlua::Result<Pair<bool>> {
    let opts = opts.as_ref();
    let Some(path) = opts.opt::<String>("path")? else {
        if resolve_language(&lang).is_none() {
            return Ok(err_pair(format!("{LANGUAGE_NOT_FOUND}: {lang}")));
        }
        return Ok((Some(true), None));
    };
    require(lua, &grants, Permission::Run)?;
    let mut loaded = LOADED_GRAMMARS
        .lock()
        .map_err(|_| mlua::Error::runtime("grammar registry lock poisoned"))?;
    if let Entry::Vacant(slot) = loaded.entry(lang) {
        let symbol = opts
            .opt::<String>("symbol_name")?
            .unwrap_or_else(|| format!("{SYMBOL_PREFIX}{}", slot.key()));
        let language = try_pair!(load_grammar(&path, &symbol));
        tracing::info!(
            lang = slot.key(),
            path,
            abi = language.abi_version(),
            "loaded tree-sitter grammar"
        );
        slot.insert(language);
    }
    Ok((Some(true), None))
}

fn load_grammar(path: &str, symbol: &str) -> Result<TsLanguage, String> {
    let path = expand_tilde(path);
    let display = path.display();
    // SAFETY: like Neovim, a parser the user points at is trusted native code.
    let library =
        unsafe { Library::new(&path) }.map_err(|e| format!("{LOAD_FAILED} {display}: {e}"))?;
    let language = {
        // SAFETY: tree-sitter parsers export `tree_sitter_<lang>` with this signature.
        let constructor: Symbol<LanguageConstructor> = unsafe { library.get(symbol.as_bytes()) }
            .map_err(|e| format!("no {symbol} in {display}: {e}"))?;
        // SAFETY: the constructor returns a static `TSLanguage` living in `library`.
        TsLanguage::new(unsafe { LanguageFn::from_raw(*constructor) })
    };
    let abi = language.abi_version();
    if !(MIN_COMPATIBLE_LANGUAGE_VERSION..=LANGUAGE_VERSION).contains(&abi) {
        return Err(format!(
            "{display} has ABI {abi}, supported are {MIN_COMPATIBLE_LANGUAGE_VERSION} to {LANGUAGE_VERSION}"
        ));
    }
    // The language points into the library, so it stays mapped for good.
    mem::forget(library);
    Ok(language)
}

/// A built-in grammar, or one `add` loaded from a path.
pub(crate) fn resolve_language(lang: &str) -> Option<TsLanguage> {
    if let Some(builtin) = Language::from_name(lang) {
        return Some(builtin.ts_language());
    }
    LOADED_GRAMMARS
        .lock()
        .ok()
        .and_then(|loaded| loaded.get(lang).cloned())
}

/// Associates {lang} with one or more filetypes, so you can look up the right
/// parser language for a given filetype later with `get_lang()`.
///
/// @param lang string Language name.
/// @param filetype string|table A single filetype string or an array of filetype strings.
/// @example
/// maki.treesitter.language.register("tsx", { "tsx", "jsx" })
#[lua_fn]
fn register(
    _lua: &Lua,
    #[ctx] reg: Arc<Mutex<LangRegistry>>,
    #[ctx] plugin: Arc<str>,
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
        maki_highlight::treesitter::alias(&plugin, ft, &lang);
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
/// maki.treesitter.language.register("tsx", { "tsx", "jsx" })
/// local lang = maki.treesitter.language.get_lang("jsx") -- "tsx"
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
    if resolve_language(&filetype).is_some() {
        return Ok(Some(filetype));
    }
    Ok(None)
}

/// Returns all filetypes that have been registered for {lang}.
///
/// @param lang string Language name.
/// @return (table) Array of filetype strings.
/// @example
/// local fts = maki.treesitter.language.get_filetypes("tsx")
/// -- { "tsx", "jsx" }
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
    let language = resolve_language(&lang)
        .ok_or_else(|| mlua::Error::runtime(format!("{LANGUAGE_NOT_FOUND}: {lang}")))?;

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
    extend "maki.treesitter.language" => fn language_fns(reg: Arc<Mutex<LangRegistry>>, grants: PluginPermissions, plugin: Arc<str>), DOCS [
        add(grants), register(reg, plugin), get_lang(reg), get_filetypes(reg), inspect,
    ]
}

pub(crate) fn create_language_module(
    lua: &Lua,
    grants: &PluginPermissions,
    plugin: Arc<str>,
) -> mlua::Result<Table> {
    let reg = Arc::new(Mutex::new(LangRegistry {
        lang_to_filetypes: HashMap::new(),
        filetype_to_lang: HashMap::new(),
    }));
    let t = lua.create_table()?;
    language_fns(&t, lua, reg, grants.clone(), plugin)?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use mlua::{Function, Lua};

    use super::*;
    use crate::plugin_permissions::PERMISSION_DENIED;

    const MISSING_PARSER: &str = "/nonexistent/parser/nu.so";
    const PLUGIN: &str = "language_test";

    fn add_fn(lua: &Lua, grants: &PluginPermissions) -> Function {
        create_language_module(lua, grants, Arc::from(PLUGIN))
            .unwrap()
            .get("add")
            .unwrap()
    }

    fn path_opts(lua: &Lua) -> Table {
        let opts = lua.create_table().unwrap();
        opts.set("path", MISSING_PARSER).unwrap();
        opts
    }

    #[test]
    fn add_without_a_path_confirms_a_built_in_grammar_without_run() {
        let lua = Lua::new();
        let add = add_fn(&lua, &PluginPermissions::denied());
        let answer: Pair<bool> = add.call("lua").unwrap();
        assert_eq!(answer, (Some(true), None));
    }

    #[test]
    fn add_without_a_path_rejects_an_unknown_grammar() {
        let lua = Lua::new();
        let add = add_fn(&lua, &PluginPermissions::denied());
        let (ok, err): Pair<bool> = add.call("cobol").unwrap();
        assert_eq!(ok, None);
        assert!(err.unwrap().starts_with(LANGUAGE_NOT_FOUND));
    }

    #[test]
    fn add_with_a_path_needs_run() {
        let lua = Lua::new();
        let add = add_fn(&lua, &PluginPermissions::denied());
        let err = add
            .call::<mlua::MultiValue>(("nu", path_opts(&lua)))
            .unwrap_err();
        assert!(err.to_string().contains(PERMISSION_DENIED), "{err}");
    }

    #[test]
    fn add_with_a_missing_library_answers_an_error() {
        let lua = Lua::new();
        let add = add_fn(&lua, &PluginPermissions::trusted());
        let (ok, err): Pair<bool> = add.call(("nu_missing", path_opts(&lua))).unwrap();
        assert_eq!(ok, None);
        assert!(err.unwrap().starts_with(LOAD_FAILED));
        assert!(resolve_language("nu_missing").is_none());
    }
}
