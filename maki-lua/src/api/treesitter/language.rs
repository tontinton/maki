use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::language::Language;
use mlua::{Lua, Table, Value as LuaValue};

struct LangRegistry {
    lang_to_filetypes: HashMap<String, Vec<String>>,
    filetype_to_lang: HashMap<String, String>,
}

pub(crate) fn create_language_module(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;

    let registry = Arc::new(Mutex::new(LangRegistry {
        lang_to_filetypes: HashMap::new(),
        filetype_to_lang: HashMap::new(),
    }));

    t.set(
        "add",
        lua.create_function(move |_, (lang, opts): (String, Option<Table>)| {
            if let Some(ref opts) = opts {
                if opts.contains_key("path")? {
                    return Err(mlua::Error::runtime(
                        "custom grammar paths not supported yet",
                    ));
                }
            }
            if Language::from_name(&lang).is_none() {
                return Err(mlua::Error::runtime(format!("language not found: {lang}")));
            }
            Ok(())
        })?,
    )?;

    let reg = Arc::clone(&registry);
    t.set(
        "register",
        lua.create_function(
            move |_, (lang, filetype_or_filetypes): (String, LuaValue)| {
                let filetypes: Vec<String> = match filetype_or_filetypes {
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
            },
        )?,
    )?;

    let reg = Arc::clone(&registry);
    t.set(
        "get_lang",
        lua.create_function(move |_, filetype: String| {
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
        })?,
    )?;

    let reg = registry;
    t.set(
        "get_filetypes",
        lua.create_function(move |lua, lang: String| {
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
        })?,
    )?;

    t.set(
        "inspect",
        lua.create_function(move |lua, lang_name: String| {
            let lang = Language::from_name(&lang_name)
                .ok_or_else(|| mlua::Error::runtime(format!("language not found: {lang_name}")))?;
            let language = lang.ts_language();

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
        })?,
    )?;

    Ok(t)
}
