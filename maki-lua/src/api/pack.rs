//! Global package declarations and read-only package state.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use maki_lua_macro::{lua_fn, lua_table};
use maki_pack::Spec;
use mlua::{Lua, MultiValue, RegistryKey, Result as LuaResult, Table, Value as LuaValue};

/// What `pack.add` and `packadd` refuse once startup has loaded the declared
/// set: nothing reads either list again, so a late call has to say so rather
/// than record something that will never happen.
const AFTER_LOAD: &str = "packages have already been loaded";
/// A name is one package. Two sources under it are a mistake in `init.lua`,
/// not a preference to resolve silently.
const ALREADY_DECLARED: &str = "is already declared";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadMode {
    Eager,
    Dormant,
    Custom(Arc<RegistryKey>),
}

#[derive(Debug, Clone)]
pub struct Declared {
    pub spec: Spec,
    pub load: LoadMode,
    pub confirm: bool,
    pub data: Option<Arc<RegistryKey>>,
}

#[derive(Debug, Default, Clone)]
pub struct PackDeclarations {
    pub specs: Vec<Declared>,
    pub active: BTreeSet<String>,
    pub pending: Vec<PackOp>,
    pub drained: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackOp {
    Activate { name: String },
}

pub type PackStore = Arc<Mutex<PackDeclarations>>;

fn reject_unknown_fields(table: &Table, allowed: &[&str], context: &str) -> LuaResult<()> {
    for pair in table.clone().pairs::<LuaValue, LuaValue>() {
        let (key, _) = pair?;
        let LuaValue::String(key) = key else {
            return Err(mlua::Error::runtime(format!(
                "{context}: field names must be strings"
            )));
        };
        let key = key.to_str()?;
        if !allowed.contains(&key.as_ref()) {
            return Err(mlua::Error::runtime(format!(
                "{context}: unknown field {key:?}"
            )));
        }
    }
    Ok(())
}

fn finish(spec: Spec) -> LuaResult<Spec> {
    if maki_pack::git::http_source_has_userinfo(&spec.src) {
        let source = crate::pack::sanitize_message(&maki_pack::git::redact(&spec.src));
        return Err(mlua::Error::runtime(format!(
            "pack.add: source {source:?} contains HTTP credentials; use a Git credential helper instead"
        )));
    }
    if !maki_pack::name_is_safe(&spec.name) {
        let source = crate::pack::sanitize_message(&maki_pack::git::redact(&spec.src));
        return Err(mlua::Error::runtime(format!(
            "pack.add: {:?} is not a usable package name for {source:?}; set 'name' explicitly",
            spec.name
        )));
    }
    if crate::loader::is_bundled(&spec.name) {
        return Err(mlua::Error::runtime(format!(
            "pack.add: {:?} is the name of a builtin plugin; set 'name' to something else",
            spec.name
        )));
    }
    Ok(spec)
}

fn parse_spec(lua: &Lua, value: LuaValue) -> LuaResult<(Spec, Option<Arc<RegistryKey>>)> {
    let table = match value {
        LuaValue::String(source) => {
            return finish(Spec::new(source.to_str()?.to_owned())).map(|spec| (spec, None));
        }
        LuaValue::Table(table) => table,
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.add: each spec must be a string or a table, got {}",
                other.type_name()
            )));
        }
    };
    reject_unknown_fields(&table, &["src", "name", "version", "data"], "pack.add spec")?;

    let source = match table.get::<LuaValue>("src")? {
        LuaValue::String(source) => source.to_str()?.to_owned(),
        LuaValue::Nil => return Err(mlua::Error::runtime("pack.add: spec is missing 'src'")),
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.add: 'src' must be a string, got {}",
                other.type_name()
            )));
        }
    };
    if source.trim().is_empty() {
        return Err(mlua::Error::runtime("pack.add: 'src' must not be empty"));
    }

    let mut spec = Spec::new(source);
    match table.get::<LuaValue>("name")? {
        LuaValue::Nil => {}
        LuaValue::String(name) => {
            let name = name.to_str()?.to_owned();
            if name.is_empty() {
                return Err(mlua::Error::runtime("pack.add: 'name' must not be empty"));
            }
            spec = spec.with_name(name);
        }
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.add: 'name' must be a string, got {}",
                other.type_name()
            )));
        }
    }
    match table.get::<LuaValue>("version")? {
        LuaValue::Nil => {}
        LuaValue::String(version) => {
            let version = version.to_str()?.to_owned();
            if version.trim().is_empty() {
                return Err(mlua::Error::runtime(
                    "pack.add: 'version' must not be empty",
                ));
            }
            spec = spec.with_version(version);
        }
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.add: 'version' must be a string, got {}",
                other.type_name()
            )));
        }
    }
    let data = match table.get::<LuaValue>("data")? {
        LuaValue::Nil => None,
        value => Some(Arc::new(lua.create_registry_value(value)?)),
    };
    finish(spec).map(|spec| (spec, data))
}

/// Declare global packages after the global `init.lua` finishes.
///
/// @param specs table Sources or tables with `src`, `name`, `version`, and `data`.
/// @param opts table? `confirm` controls source confirmation. `load` is a
///   boolean or a custom loader function.
/// @example
/// maki.pack.add({
///   { src = "https://github.com/user/maki-goal", version = "main" },
/// })
#[lua_fn]
fn add(lua: &Lua, specs: Table, opts: Option<Table>) -> LuaResult<()> {
    let store = lua
        .app_data_ref::<PackStore>()
        .ok_or_else(|| mlua::Error::runtime("pack.add: not available here"))?
        .clone();
    let (load, confirm) = parse_options(lua, opts)?;
    let mut parsed = Vec::new();
    for entry in specs.sequence_values::<LuaValue>() {
        let (spec, data) = parse_spec(lua, entry?)?;
        parsed.push(Declared {
            spec,
            load: load.clone(),
            confirm,
            data,
        });
    }

    let mut declarations = store.lock().expect("pack declarations");
    if declarations.drained {
        return Err(mlua::Error::runtime(format!("pack.add: {AFTER_LOAD}")));
    }
    for declared in parsed {
        let existing = declarations
            .specs
            .iter()
            .find(|existing| existing.spec.name == declared.spec.name);
        match existing {
            Some(existing) if existing.spec != declared.spec => {
                let source =
                    crate::pack::sanitize_message(&maki_pack::git::redact(&existing.spec.src));
                return Err(mlua::Error::runtime(format!(
                    "pack.add: {:?} {ALREADY_DECLARED} for {source:?}",
                    declared.spec.name
                )));
            }
            Some(_) => {}
            None => declarations.specs.push(declared),
        }
    }
    Ok(())
}

fn parse_options(lua: &Lua, opts: Option<Table>) -> LuaResult<(LoadMode, bool)> {
    let Some(opts) = opts else {
        return Ok((LoadMode::Eager, true));
    };
    reject_unknown_fields(&opts, &["confirm", "load"], "pack.add")?;
    let confirm = match opts.get::<LuaValue>("confirm")? {
        LuaValue::Nil => true,
        LuaValue::Boolean(confirm) => confirm,
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.add: 'confirm' must be a boolean, got {}",
                other.type_name()
            )));
        }
    };
    let load = match opts.get::<LuaValue>("load")? {
        LuaValue::Nil | LuaValue::Boolean(true) => LoadMode::Eager,
        LuaValue::Boolean(false) => LoadMode::Dormant,
        LuaValue::Function(function) => {
            LoadMode::Custom(Arc::new(lua.create_registry_value(function)?))
        }
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.add: 'load' must be a boolean or function, got {}",
                other.type_name()
            )));
        }
    };
    Ok((load, confirm))
}

fn enqueue(lua: &Lua, name: String) -> LuaResult<()> {
    if !maki_pack::name_is_safe(&name) {
        return Err(mlua::Error::runtime(format!(
            "pack: {name:?} is not a usable package name"
        )));
    }
    let store = lua
        .app_data_ref::<PackStore>()
        .ok_or_else(|| mlua::Error::runtime("pack: not available here"))?
        .clone();
    let mut declarations = store.lock().expect("pack declarations");
    if declarations.drained {
        return Err(mlua::Error::runtime(format!("maki.packadd: {AFTER_LOAD}")));
    }
    let operation = PackOp::Activate { name };
    if !declarations.pending.contains(&operation) {
        declarations.pending.push(operation);
    }
    Ok(())
}

/// Load an installed package that is not active.
///
/// @param name string Package name.
#[lua_fn]
fn packadd(lua: &Lua, name: String) -> LuaResult<()> {
    enqueue(lua, name)
}

pub(crate) fn add_packadd(lua: &Lua, maki: &Table) -> LuaResult<()> {
    maki.set("packadd", lua.create_function(packadd)?)
}

/// Get package state without changing the installed set.
///
/// @param names table? Package names. Omit for all managed packages.
/// @param opts table? Reserved. Omit it.
/// @return (table) Package records with `spec`, `path`, `rev`, and `active`.
#[lua_fn]
fn get(lua: &Lua, names: Option<Table>, opts: Option<Table>) -> LuaResult<Table> {
    if let Some(opts) = opts {
        reject_unknown_fields(&opts, &[], "pack.get")?;
    }
    let requested: Option<Vec<String>> = names
        .map(|table| table.sequence_values::<String>().collect())
        .transpose()?;
    let store = lua
        .app_data_ref::<PackStore>()
        .ok_or_else(|| mlua::Error::runtime("pack.get: not available here"))?
        .clone();
    let declarations = store.lock().expect("pack declarations").clone();
    let lock = crate::pack::read_lockfile(crate::pack::lockfile_path().as_deref())
        .ok_or_else(|| mlua::Error::runtime("pack.get: pack lockfile is unreadable"))?;
    let site = crate::pack::site_dir().ok();
    package_state_table(lua, requested, &declarations, &lock, site.as_deref())
}

fn package_state_table(
    lua: &Lua,
    requested: Option<Vec<String>>,
    declarations: &PackDeclarations,
    lock: &maki_pack::lockfile::Lockfile,
    site: Option<&std::path::Path>,
) -> LuaResult<Table> {
    let names = requested.unwrap_or_else(|| {
        let mut names: Vec<String> = declarations
            .specs
            .iter()
            .map(|declared| declared.spec.name.clone())
            .collect();
        let extras: Vec<String> = lock
            .install_order()
            .filter(|name| !names.iter().any(|known| known == *name))
            .map(str::to_owned)
            .collect();
        names.extend(extras);
        names
    });
    let manager = site.map(maki_pack::manager::Manager::new);
    let result = lua.create_table()?;
    for (index, name) in names.into_iter().enumerate() {
        let declared = declarations
            .specs
            .iter()
            .find(|declared| declared.spec.name == name);
        let locked = lock
            .get(&name)
            .filter(|entry| declared.is_none_or(|declared| declared.spec.src == entry.src));
        if declared.is_none() && locked.is_none() {
            return Err(mlua::Error::runtime(format!(
                "pack.get: package {name:?} is not installed"
            )));
        }
        let fallback;
        let spec = match declared {
            Some(declared) => &declared.spec,
            None => {
                let entry = locked.expect("checked above");
                fallback = Spec::new(entry.src.clone()).with_name(name.clone());
                &fallback
            }
        };
        let item = lua.create_table()?;
        item.set(
            "spec",
            spec_to_lua(
                lua,
                spec,
                declared.and_then(|declared| declared.data.as_ref()),
            )?,
        )?;
        item.set(
            "path",
            manager
                .as_ref()
                .and_then(|manager| locked.and_then(|_| manager.resolve(lock, &name)))
                .map(|path| path.display().to_string()),
        )?;
        item.set("rev", locked.map(|entry| entry.rev.as_str()))?;
        item.set("active", declarations.active.contains(&name))?;
        result.set(index + 1, item)?;
    }
    Ok(result)
}

pub(crate) fn spec_to_lua(
    lua: &Lua,
    spec: &Spec,
    data: Option<&Arc<RegistryKey>>,
) -> LuaResult<Table> {
    let table = lua.create_table()?;
    table.set("src", maki_pack::git::redact(&spec.src))?;
    table.set("name", spec.name.as_str())?;
    if let Some(version) = &spec.version {
        table.set("version", version.as_str())?;
    }
    if let Some(data) = data {
        table.set("data", lua.registry_value::<LuaValue>(data.as_ref())?)?;
    }
    Ok(table)
}

pub(crate) fn create_pack_read_table(lua: &Lua) -> LuaResult<Table> {
    let table = lua.create_table()?;
    table.set(
        "add",
        lua.create_function(|_, _: MultiValue| -> LuaResult<()> {
            Err(mlua::Error::runtime(
                "maki.pack.add is only available in the global init.lua",
            ))
        })?,
    )?;
    table.set(
        "get",
        lua.create_function(|lua, (names, opts): (Option<Table>, Option<Table>)| {
            get(lua, names, opts)
        })?,
    )?;
    Ok(table)
}

lua_table! {
    /// Declare global packages and inspect package state.
    ///
    /// `add` is available only in the global `init.lua`. `get` is read-only
    /// and is available in project config and packages.
    "maki.pack" => pub(crate) fn create_pack_table(), DOCS [
        add, get,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const TEST_REV: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn lua_with_store() -> (Lua, PackStore) {
        let lua = Lua::new();
        let store: PackStore = Arc::default();
        lua.set_app_data(store.clone());
        (lua, store)
    }

    fn add_fn(lua: &Lua) -> mlua::Function {
        create_pack_table(lua).unwrap().get("add").unwrap()
    }

    #[test]
    fn a_table_spec_carries_name_version_and_data() {
        let (lua, store) = lua_with_store();
        let data = lua.create_table().unwrap();
        data.set("number", 7).unwrap();
        let spec = lua.create_table().unwrap();
        spec.set("src", "https://github.com/user/repo").unwrap();
        spec.set("name", "custom").unwrap();
        spec.set("version", "v1.2.3").unwrap();
        spec.set("data", data).unwrap();
        add_fn(&lua)
            .call::<()>(lua.create_sequence_from([spec]).unwrap())
            .unwrap();

        let declared = store.lock().unwrap().specs[0].clone();
        assert_eq!(declared.spec.name, "custom");
        assert_eq!(declared.spec.version.as_deref(), Some("v1.2.3"));
        let output = spec_to_lua(&lua, &declared.spec, declared.data.as_ref()).unwrap();
        let output_data: Table = output.get("data").unwrap();
        assert_eq!(output_data.get::<i64>("number").unwrap(), 7);
    }

    #[test]
    fn a_version_table_is_rejected() {
        let (lua, _) = lua_with_store();
        let spec = lua.create_table().unwrap();
        spec.set("src", "https://github.com/user/repo").unwrap();
        spec.set("version", lua.create_table().unwrap()).unwrap();

        assert!(
            add_fn(&lua)
                .call::<()>(lua.create_sequence_from([spec]).unwrap())
                .is_err()
        );
    }

    #[test]
    fn custom_loader_is_retained_in_the_registry() {
        let (lua, store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://github.com/user/repo"])
            .unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("load", lua.create_function(|_, ()| Ok(())).unwrap())
            .unwrap();
        add_fn(&lua).call::<()>((specs, opts)).unwrap();

        assert!(matches!(
            store.lock().unwrap().specs[0].load,
            LoadMode::Custom(_)
        ));
    }

    /// Re-running the same declaration is how a shared `init.lua` snippet
    /// behaves, so it stays a no-op rather than an error.
    #[test]
    fn an_identical_second_declaration_of_a_name_wins() {
        let (lua, store) = lua_with_store();
        let source = "https://example.com/one/repo";
        for _ in 0..2 {
            add_fn(&lua)
                .call::<()>(lua.create_sequence_from([source]).unwrap())
                .unwrap();
        }

        let declarations = store.lock().unwrap();
        assert_eq!(declarations.specs.len(), 1);
        assert_eq!(declarations.specs[0].spec.src, source);
    }

    #[test_case("https://example.com/two/repo", None ; "another_source")]
    #[test_case("https://example.com/one/repo", Some("v2") ; "another_version")]
    fn a_contradicting_declaration_of_a_name_is_refused(source: &str, version: Option<&str>) {
        let (lua, store) = lua_with_store();
        add_fn(&lua)
            .call::<()>(
                lua.create_sequence_from(["https://example.com/one/repo"])
                    .unwrap(),
            )
            .unwrap();

        let spec = lua.create_table().unwrap();
        spec.set("src", source).unwrap();
        if let Some(version) = version {
            spec.set("version", version).unwrap();
        }
        let error = add_fn(&lua)
            .call::<()>(lua.create_sequence_from([spec]).unwrap())
            .expect_err("a second source for one name must be refused")
            .to_string();

        assert!(
            error.contains(ALREADY_DECLARED),
            "unexpected error: {error}"
        );
        assert_eq!(store.lock().unwrap().specs.len(), 1);
    }

    /// Nothing reads the declared set after startup, so a late `pack.add` has
    /// to fail the way `packadd` does instead of recording a dead entry.
    #[test]
    fn adding_after_the_declared_set_was_loaded_is_refused() {
        let (lua, store) = lua_with_store();
        store.lock().unwrap().drained = true;

        let error = add_fn(&lua)
            .call::<()>(
                lua.create_sequence_from(["https://example.com/one/repo"])
                    .unwrap(),
            )
            .expect_err("a declaration after the load must be refused")
            .to_string();

        assert!(error.contains(AFTER_LOAD), "unexpected error: {error}");
        assert!(store.lock().unwrap().specs.is_empty());
    }

    #[test]
    fn packadd_queues_one_activation() {
        let (lua, store) = lua_with_store();
        let maki = lua.create_table().unwrap();
        add_packadd(&lua, &maki).unwrap();
        let packadd: mlua::Function = maki.get("packadd").unwrap();

        packadd.call::<()>("demo").unwrap();
        packadd.call::<()>("demo").unwrap();

        assert_eq!(
            store.lock().unwrap().pending,
            vec![PackOp::Activate {
                name: "demo".to_owned()
            }]
        );
    }

    #[test]
    fn get_preserves_order_and_reports_exact_state() {
        let (lua, store) = lua_with_store();
        let zeta = lua.create_table().unwrap();
        zeta.set("src", "https://example.com/zeta").unwrap();
        zeta.set("data", "kept").unwrap();
        let alpha = lua.create_table().unwrap();
        alpha.set("src", "https://example.com/alpha").unwrap();
        add_fn(&lua)
            .call::<()>(lua.create_sequence_from([zeta, alpha]).unwrap())
            .unwrap();
        store.lock().unwrap().active.insert("alpha".to_owned());

        let site = tempfile::TempDir::new().unwrap();
        let mut lock = maki_pack::lockfile::Lockfile::default();
        lock.record("alpha", "https://example.com/alpha", TEST_REV);
        lock.record("legacy", "https://example.com/legacy", TEST_REV);
        for name in ["alpha", "legacy"] {
            std::fs::create_dir_all(maki_pack::paths::revision_dir(site.path(), name, TEST_REV))
                .unwrap();
        }
        let declarations = store.lock().unwrap().clone();

        let all = package_state_table(&lua, None, &declarations, &lock, Some(site.path())).unwrap();
        assert_eq!(all.raw_len(), 3);
        assert_eq!(
            all.get::<Table>(1)
                .unwrap()
                .get::<Table>("spec")
                .unwrap()
                .get::<String>("name")
                .unwrap(),
            "zeta"
        );
        assert_eq!(
            all.get::<Table>(3)
                .unwrap()
                .get::<Table>("spec")
                .unwrap()
                .get::<String>("name")
                .unwrap(),
            "legacy"
        );

        let filtered = package_state_table(
            &lua,
            Some(vec!["alpha".to_owned(), "zeta".to_owned()]),
            &declarations,
            &lock,
            Some(site.path()),
        )
        .unwrap();
        let alpha: Table = filtered.get(1).unwrap();
        assert!(alpha.get::<bool>("active").unwrap());
        assert_eq!(alpha.get::<String>("rev").unwrap(), TEST_REV);
        assert_eq!(
            alpha.get::<String>("path").unwrap(),
            maki_pack::paths::revision_dir(site.path(), "alpha", TEST_REV)
                .display()
                .to_string()
        );
        let zeta: Table = filtered.get(2).unwrap();
        let zeta_spec: Table = zeta.get("spec").unwrap();
        assert_eq!(zeta_spec.get::<String>("data").unwrap(), "kept");
        assert!(zeta.get::<Option<String>>("rev").unwrap().is_none());
        assert!(!zeta.get::<bool>("active").unwrap());
    }

    #[test]
    fn get_rejects_missing_names_and_hides_replaced_source_state() {
        let (lua, store) = lua_with_store();
        add_fn(&lua)
            .call::<()>(
                lua.create_sequence_from(["https://example.com/new/demo"])
                    .unwrap(),
            )
            .unwrap();
        let declarations = store.lock().unwrap().clone();
        let mut lock = maki_pack::lockfile::Lockfile::default();
        lock.record("demo", "https://example.com/old/demo", TEST_REV);

        let replaced = package_state_table(
            &lua,
            Some(vec!["demo".to_owned()]),
            &declarations,
            &lock,
            None,
        )
        .unwrap();
        let item: Table = replaced.get(1).unwrap();
        assert!(item.get::<Option<String>>("path").unwrap().is_none());
        assert!(item.get::<Option<String>>("rev").unwrap().is_none());

        let error = package_state_table(
            &lua,
            Some(vec!["missing".to_owned()]),
            &declarations,
            &lock,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("not installed"), "{error}");
    }
}
