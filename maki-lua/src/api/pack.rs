//! `maki.pack`, modelled after Neovim's `vim.pack`.
//!
//! Declaring a package records it and returns. Nothing is cloned or loaded from
//! inside the call, because `add` runs while `init.lua` is being sourced and
//! every load path blocks on a reply from the same runtime thread the caller is
//! occupying. The host installs and loads the recorded set afterwards, which is
//! also the phase Neovim defers to.

use std::sync::{Arc, Mutex};

use maki_lua_macro::{lua_fn, lua_table};
use maki_pack::{Spec, Version};
use mlua::{Lua, Result as LuaResult, Table, Value as LuaValue};

/// When a declared package should load.
///
/// The default follows Neovim: `false` while `init.lua` is being sourced, and
/// `true` afterwards. Maki's startup has a phase after the init files that
/// corresponds to the one Neovim defers to, so a package declared with no
/// `load` still loads, just at that phase rather than inside the `add` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadMode {
    /// Load in the startup package phase.
    Eager,
    /// Installed and recorded, but not loaded until something activates it.
    ///
    /// Maki diverges from Neovim here on purpose. There, `load = false` means
    /// `:packadd!`, and startup sources the package anyway, so the value barely
    /// differs from `true`. A lazy trigger needs a state to delay, and Neovim
    /// has no lazy loading, so it never needed one.
    Dormant,
    /// Dormant until one of these fires, then loaded once.
    Triggered(Triggers),
}

/// What wakes a dormant package.
///
/// Event, command, and keymap are the lazy.nvim concepts that map onto maki.
/// There is deliberately no module trigger: `require` resolves against the
/// calling plugin's own root and binds the chunk to the caller's environment,
/// so activating another package on a module miss would run downloaded code
/// under whichever owner happened to require it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Triggers {
    /// Host-fired event names. A plugin-fired `exec_autocmds` does not
    /// activate a package, because that dispatch is synchronous and loading is
    /// not.
    pub event: Vec<String>,
    /// Slash command names, with or without the leading slash.
    pub cmd: Vec<String>,
    /// Keys in Vim notation.
    pub keys: Vec<String>,
}

impl Triggers {
    pub fn is_empty(&self) -> bool {
        self.event.is_empty() && self.cmd.is_empty() && self.keys.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Declared {
    pub spec: Spec,
    pub load: LoadMode,
    /// Ask before cloning this source. Cleared with `confirm = false`, which
    /// is a statement that the source is already trusted, and never an
    /// approval of the permissions the package asks for.
    pub confirm: bool,
}

/// Everything `init.lua` declared, in declaration order.
///
/// Session state only. What is installed, and at which revision, lives in the
/// lockfile, and this never holds a second opinion about it.
#[derive(Debug, Default, Clone)]
pub struct PackDeclarations {
    pub specs: Vec<Declared>,
    /// Operations recorded by Lua for the host to perform after the calling
    /// task has exited. Nothing here runs inline: `update` and `del` unload an
    /// owner, and unloading blocks on a reply from the runtime thread the
    /// caller is occupying.
    pub pending: Vec<PackOp>,
}

/// Where an update should move a package to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpdateTarget {
    /// Resolve `version` again and take what it points at now.
    #[default]
    Version,
    /// Go back to the revision the lockfile records. This is the undo for an
    /// update that moved to something broken.
    Lockfile,
}

/// How an update should behave. One type, shared by `maki.pack.update` and by
/// `/packupdate`, so a flag cannot mean one thing from Lua and another from
/// the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpdateOptions {
    pub target: UpdateTarget,
    /// Do not contact the remote. Resolves against refs already on disk.
    pub offline: bool,
}

impl UpdateOptions {
    /// Narrows a remote policy to what `offline` allows. Offline always wins,
    /// so no combination of options can reach the network against the user's
    /// explicit instruction.
    pub fn remote(&self, wanted: maki_pack::manager::Refresh) -> maki_pack::manager::Refresh {
        if self.offline {
            maki_pack::manager::Refresh::Never
        } else {
            wanted
        }
    }
}

/// A change to the installed set, requested from Lua.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackOp {
    /// Fetch and move a package to what its version now resolves to.
    Update {
        name: String,
        options: UpdateOptions,
    },
    /// Remove a package and its lockfile entry.
    Delete { name: String },
    /// Load a package that is installed but dormant.
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

/// The one place a spec's name is accepted, whichever form it arrived in.
///
/// The name becomes a directory under the package root, so it is refused here
/// rather than reaching the filesystem.
fn finish(spec: Spec) -> LuaResult<Spec> {
    if maki_pack::git::http_source_has_userinfo(&spec.src) {
        let source = crate::pack::sanitize_message(&maki_pack::git::redact(&spec.src));
        return Err(mlua::Error::runtime(format!(
            "pack.add: source {:?} contains HTTP credentials; use a Git credential helper instead",
            source
        )));
    }
    if !maki_pack::name_is_safe(&spec.name) {
        let source = crate::pack::sanitize_message(&maki_pack::git::redact(&spec.src));
        return Err(mlua::Error::runtime(format!(
            "pack.add: {:?} is not a usable package name for {:?}; set 'name' explicitly",
            spec.name, source
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

/// Reads a spec entry, which is either a string source or a table.
fn parse_spec(value: LuaValue) -> LuaResult<Spec> {
    let table = match value {
        // Neovim accepts a bare string and treats it as `src`. It still goes
        // through the name check below: a source whose name cannot be derived
        // is no safer for having been written as a string.
        LuaValue::String(s) => return finish(Spec::new(s.to_str()?.to_owned())),
        LuaValue::Table(t) => t,
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.add: each spec must be a string or a table, got {}",
                other.type_name()
            )));
        }
    };
    reject_unknown_fields(&table, &["src", "name", "version"], "pack.add spec")?;

    let src = match table.get::<LuaValue>("src")? {
        LuaValue::String(src) => src.to_str()?.to_owned(),
        LuaValue::Nil => return Err(mlua::Error::runtime("pack.add: spec is missing 'src'")),
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.add: 'src' must be a string, got {}",
                other.type_name()
            )));
        }
    };
    if src.trim().is_empty() {
        return Err(mlua::Error::runtime("pack.add: 'src' must not be empty"));
    }

    let mut spec = Spec::new(src);
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
    spec =
        match table.get::<LuaValue>("version")? {
            LuaValue::Nil => spec,
            LuaValue::String(s) => {
                let revision = s.to_str()?.to_owned();
                if revision.trim().is_empty() {
                    return Err(mlua::Error::runtime(
                        "pack.add: 'version' must not be empty",
                    ));
                }
                spec.with_version(Version::Rev(revision))
            }
            LuaValue::Table(t) => {
                let raw: String = t.get(super::version::RANGE_MARKER).map_err(|_| {
                    mlua::Error::runtime(
                        "pack.add: 'version' table must come from maki.version.range()",
                    )
                })?;
                spec.with_version(Version::range(&raw).map_err(|e| {
                    mlua::Error::runtime(format!("pack.add: bad version range: {e}"))
                })?)
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "pack.add: 'version' must be a string or maki.version.range(), got {}",
                    other.type_name()
                )));
            }
        };

    finish(spec)
}

/// Declare packages to install and load, like `vim.pack.add`.
///
/// Recording is all this does. The host installs and loads the declared set
/// after `init.lua` finishes, so a slow clone never blocks the call and a
/// package failure never stops maki from starting.
///
/// Only available inside `init.lua`. Declaring a package fetches code, so it is
/// configuration rather than something a downloaded plugin may do.
///
/// @param specs table List of specs. Each is a source string, or a table with
///   `src` (string, required), `name` (string), and `version` (string or
///   `maki.version.range()`).
/// @param opts table? Options: `load` (boolean|table) `false` installs the
///   package without loading it, leaving it for `maki.packadd`; a table of
///   `event`, `cmd`, and `keys` loads it the first time one of those fires.
///   `confirm` (boolean) `false` clones a new source without asking, which
///   states that the source is trusted and never approves its permissions.
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

    let (load, confirm) = parse_options(opts)?;

    let mut parsed = Vec::new();
    for entry in specs.sequence_values::<LuaValue>() {
        parsed.push(Declared {
            spec: parse_spec(entry?)?,
            load: load.clone(),
            confirm,
        });
    }

    let mut declarations = store.lock().expect("pack declarations");
    for declared in parsed {
        // Neovim keeps the first declaration of a name for the session.
        if !declarations
            .specs
            .iter()
            .any(|existing| existing.spec.name == declared.spec.name)
        {
            declarations.specs.push(declared);
        }
    }
    Ok(())
}

/// Reads `opts.load`.
///
/// `nil` means the default, which is to load in the startup phase. `false`
/// leaves the package dormant until something activates it.
fn parse_options(opts: Option<Table>) -> LuaResult<(LoadMode, bool)> {
    let Some(opts) = opts else {
        return Ok((LoadMode::Eager, true));
    };
    reject_unknown_fields(&opts, &["load", "confirm"], "pack.add")?;
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
        LuaValue::Nil => LoadMode::Eager,
        LuaValue::Boolean(true) => LoadMode::Eager,
        LuaValue::Boolean(false) => LoadMode::Dormant,
        LuaValue::Table(t) => parse_triggers(&t)?,
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.add: 'load' must be a boolean or a trigger table, got {}",
                other.type_name()
            )));
        }
    };
    Ok((load, confirm))
}

fn parse_triggers(table: &Table) -> LuaResult<LoadMode> {
    reject_unknown_fields(table, &["event", "cmd", "keys"], "pack.add load")?;
    let mut triggers = Triggers::default();
    for (key, target) in [
        ("event", &mut triggers.event),
        ("cmd", &mut triggers.cmd),
        ("keys", &mut triggers.keys),
    ] {
        match table.get::<LuaValue>(key)? {
            LuaValue::Nil => {}
            // Checked for the bare string too, not only for the list form.
            // An empty name counts towards "some trigger was named" while
            // matching nothing, which would leave the package dormant with
            // nothing able to wake it.
            LuaValue::String(s) => {
                let entry = s.to_str()?.to_owned();
                if entry.trim().is_empty() {
                    return Err(mlua::Error::runtime(format!(
                        "pack.add: '{key}' must not be empty"
                    )));
                }
                target.push(entry);
            }
            LuaValue::Table(list) => {
                for entry in list.sequence_values::<String>() {
                    let entry = entry?;
                    if entry.trim().is_empty() {
                        return Err(mlua::Error::runtime(format!(
                            "pack.add: empty {key} trigger"
                        )));
                    }
                    target.push(entry);
                }
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "pack.add: '{key}' must be a string or a list, got {}",
                    other.type_name()
                )));
            }
        }
    }
    // An empty table would leave the package dormant with nothing able to wake
    // it, which is almost certainly a typo rather than an intent.
    if triggers.is_empty() {
        return Err(mlua::Error::runtime(
            "pack.add: 'load' table names no trigger; use event, cmd, or keys",
        ));
    }
    Ok(LoadMode::Triggered(triggers))
}

/// Records an operation for the host, rejecting an unusable name first.
fn enqueue(lua: &Lua, op: PackOp, name: &str) -> LuaResult<()> {
    if !maki_pack::name_is_safe(name) {
        return Err(mlua::Error::runtime(format!(
            "pack: {name:?} is not a usable package name"
        )));
    }
    let store = lua
        .app_data_ref::<PackStore>()
        .ok_or_else(|| mlua::Error::runtime("pack: not available here"))?
        .clone();
    let mut declarations = store.lock().expect("pack declarations");
    if !declarations.pending.contains(&op) {
        declarations.pending.push(op);
    }
    Ok(())
}

/// Update packages to what their version now resolves to, like
/// `vim.pack.update`.
///
/// The work happens after this call returns, because updating unloads and
/// reloads the package and unloading waits on the runtime this call occupies.
///
/// @param names table? Package names. Omit for every declared package.
/// @param opts table? `offline` to work without the network, and `target` of
///   `"version"` (the default) to take what version now resolves to, or
///   `"lockfile"` to go back to the recorded revision.
/// @example
/// maki.pack.update({ "maki-goal" })
/// maki.pack.update(nil, { target = "lockfile" })
#[lua_fn]
fn update(lua: &Lua, names: Option<Table>, opts: Option<Table>) -> LuaResult<()> {
    let options = parse_update_options(opts)?;
    let names = names_arg(names)?;
    // Omitting the argument means every declared package, which is what the
    // documentation has always said. Enumerating them here rather than at the
    // point of application keeps one meaning of "which packages" and lets a
    // name be validated the same way whether it was typed or implied.
    let names = if names.is_empty() {
        declared_names(lua)?
    } else {
        names
    };
    for name in names {
        enqueue(
            lua,
            PackOp::Update {
                name: name.clone(),
                options,
            },
            &name,
        )?;
    }
    Ok(())
}

/// Every package name `maki.pack.add` declared, in declaration order.
fn declared_names(lua: &Lua) -> LuaResult<Vec<String>> {
    let store = lua
        .app_data_ref::<PackStore>()
        .ok_or_else(|| mlua::Error::runtime("pack: not available here"))?
        .clone();
    let declarations = store.lock().expect("pack declarations");
    Ok(declarations
        .specs
        .iter()
        .map(|d| d.spec.name.clone())
        .collect())
}

/// Reads the option table shared by `maki.pack.update` and `/packupdate`.
fn parse_update_options(opts: Option<Table>) -> LuaResult<UpdateOptions> {
    let Some(opts) = opts else {
        return Ok(UpdateOptions::default());
    };
    let offline = match opts.get::<LuaValue>("offline")? {
        LuaValue::Nil => false,
        LuaValue::Boolean(b) => b,
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.update: 'offline' must be a boolean, got {}",
                other.type_name()
            )));
        }
    };
    let target = match opts.get::<LuaValue>("target")? {
        LuaValue::Nil => UpdateTarget::Version,
        LuaValue::String(s) => match s.to_str()?.as_ref() {
            "version" => UpdateTarget::Version,
            "lockfile" => UpdateTarget::Lockfile,
            other => {
                return Err(mlua::Error::runtime(format!(
                    "pack.update: 'target' must be \"version\" or \"lockfile\", got {other:?}"
                )));
            }
        },
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.update: 'target' must be a string, got {}",
                other.type_name()
            )));
        }
    };
    Ok(UpdateOptions { target, offline })
}

/// Remove packages, like `vim.pack.del`.
///
/// Runs after this call returns, for the same reason as `update`.
///
/// @param names table Package names to remove.
/// @example
/// maki.pack.del({ "maki-goal" })
#[lua_fn]
fn del(lua: &Lua, names: Table) -> LuaResult<()> {
    let names = names_arg(Some(names))?;
    if names.is_empty() {
        return Err(mlua::Error::runtime("pack.del: name a package to remove"));
    }
    for name in names {
        enqueue(lua, PackOp::Delete { name: name.clone() }, &name)?;
    }
    Ok(())
}

/// Activate an installed package that is not loaded, like `:packadd`.
///
/// This is how an `opt/` package, or one declared with no automatic load, is
/// brought in. It is available to any plugin, unlike `add`, `update`, and
/// `del`: activating code that is already installed and already approved is a
/// much weaker act than fetching new code.
///
/// Loading happens after this call returns, for the same reason as `update`.
///
/// @param name string Package to activate.
/// @example
/// maki.packadd("maki-goal")
#[lua_fn]
fn packadd(lua: &Lua, name: String) -> LuaResult<()> {
    enqueue(lua, PackOp::Activate { name: name.clone() }, &name)
}

/// Registers `maki.packadd` on the root table.
///
/// It sits beside the other always-available functions rather than inside
/// `maki.pack`, because that table exists only for `init.lua` while activation
/// is something any plugin may do.
pub(crate) fn add_packadd(lua: &Lua, maki: &Table) -> LuaResult<()> {
    maki.set("packadd", lua.create_function(packadd_raw)?)
}

fn packadd_raw(lua: &Lua, name: String) -> LuaResult<()> {
    packadd(lua, name)
}

fn names_arg(names: Option<Table>) -> LuaResult<Vec<String>> {
    let Some(table) = names else {
        return Ok(Vec::new());
    };
    table.sequence_values::<String>().collect()
}

lua_table! {
    /// Declaring external packages, modelled after `vim.pack`.
    ///
    /// Available only inside `init.lua`, because installing a package fetches
    /// code and that is a configuration decision rather than something a
    /// plugin may do for itself.
    ///
    /// ```lua
    /// maki.pack.add({ "https://github.com/user/maki-goal" })
    /// ```
    "maki.pack" => pub(crate) fn create_pack_table(), DOCS [
        add, update, del,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::version::create_version_table;

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
    fn a_bare_string_is_treated_as_a_source() {
        let (lua, store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://github.com/user/repo"])
            .unwrap();
        add_fn(&lua).call::<()>(specs).unwrap();

        let declared = store.lock().unwrap();
        assert_eq!(declared.specs.len(), 1);
        assert_eq!(declared.specs[0].spec.src, "https://github.com/user/repo");
        assert_eq!(declared.specs[0].spec.name, "repo", "name derived from src");
    }

    #[test]
    fn a_table_spec_carries_name_and_version() {
        let (lua, store) = lua_with_store();
        let spec = lua.create_table().unwrap();
        spec.set("src", "https://github.com/user/repo").unwrap();
        spec.set("name", "custom").unwrap();
        spec.set("version", "v1.2.3").unwrap();
        let specs = lua.create_sequence_from([spec]).unwrap();
        add_fn(&lua).call::<()>(specs).unwrap();

        let declared = store.lock().unwrap();
        assert_eq!(declared.specs[0].spec.name, "custom");
        assert_eq!(
            declared.specs[0].spec.version,
            Version::Rev("v1.2.3".to_owned())
        );
    }

    #[test]
    fn a_range_from_version_range_is_accepted() {
        let (lua, store) = lua_with_store();
        let range_fn: mlua::Function = create_version_table(&lua).unwrap().get("range").unwrap();
        let range: Table = range_fn.call("^1.2").unwrap();

        let spec = lua.create_table().unwrap();
        spec.set("src", "https://github.com/user/repo").unwrap();
        spec.set("version", range).unwrap();
        let specs = lua.create_sequence_from([spec]).unwrap();
        add_fn(&lua).call::<()>(specs).unwrap();

        assert!(matches!(
            store.lock().unwrap().specs[0].spec.version,
            Version::Range(_)
        ));
    }

    /// An arbitrary table is not a constraint, so it must not be mistaken for
    /// one and silently ignored.
    #[test]
    fn a_plain_table_version_is_rejected() {
        let (lua, _store) = lua_with_store();
        let spec = lua.create_table().unwrap();
        spec.set("src", "https://github.com/user/repo").unwrap();
        spec.set("version", lua.create_table().unwrap()).unwrap();
        let specs = lua.create_sequence_from([spec]).unwrap();
        assert!(add_fn(&lua).call::<()>(specs).is_err());
    }

    #[test]
    fn a_spec_without_src_is_rejected() {
        let (lua, _store) = lua_with_store();
        let spec = lua.create_table().unwrap();
        spec.set("name", "orphan").unwrap();
        let specs = lua.create_sequence_from([spec]).unwrap();
        assert!(add_fn(&lua).call::<()>(specs).is_err());
    }

    /// The name becomes a directory, so an unsafe one is refused here rather
    /// than reaching the filesystem.
    #[test]
    fn an_unsafe_name_is_refused() {
        let (lua, _store) = lua_with_store();
        for bad in ["../escape", "/etc", "a/b"] {
            let spec = lua.create_table().unwrap();
            spec.set("src", "https://github.com/user/repo").unwrap();
            spec.set("name", bad).unwrap();
            let specs = lua.create_sequence_from([spec]).unwrap();
            assert!(add_fn(&lua).call::<()>(specs).is_err(), "{bad} allowed");
        }
    }

    #[test]
    fn an_unsafe_name_error_redacts_source_credentials() {
        let (lua, _store) = lua_with_store();
        let spec = lua.create_table().unwrap();
        spec.set("src", "https://secret@example.com/repo").unwrap();
        spec.set("name", "../escape").unwrap();
        let specs = lua.create_sequence_from([spec]).unwrap();

        let error = add_fn(&lua).call::<()>(specs).unwrap_err().to_string();

        assert!(!error.contains("secret"));
        assert!(error.contains("https://***@example.com/repo"));
    }

    #[test]
    fn http_credentials_are_rejected_before_installation() {
        let (lua, store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://user:secret@example.com/repo"])
            .unwrap();

        let error = add_fn(&lua).call::<()>(specs).unwrap_err().to_string();

        assert!(!error.contains("secret"), "credential leaked: {error}");
        assert!(error.contains("Git credential helper"));
        assert!(store.lock().unwrap().specs.is_empty());
    }

    #[test]
    fn a_non_string_name_is_rejected() {
        let (lua, _store) = lua_with_store();
        let spec = lua.create_table().unwrap();
        spec.set("src", "https://github.com/user/repo").unwrap();
        spec.set("name", 7).unwrap();
        let specs = lua.create_sequence_from([spec]).unwrap();

        assert!(add_fn(&lua).call::<()>(specs).is_err());
    }

    #[test]
    fn an_empty_name_is_rejected() {
        let (lua, store) = lua_with_store();
        let spec = lua.create_table().unwrap();
        spec.set("src", "https://github.com/user/repo").unwrap();
        spec.set("name", "").unwrap();
        let specs = lua.create_sequence_from([spec]).unwrap();

        assert!(add_fn(&lua).call::<()>(specs).is_err());
        assert!(store.lock().unwrap().specs.is_empty());
    }

    #[test]
    fn unknown_spec_and_option_fields_are_rejected() {
        let (lua, store) = lua_with_store();
        let spec = lua.create_table().unwrap();
        spec.set("src", "https://github.com/user/repo").unwrap();
        spec.set("typo", true).unwrap();
        let specs = lua.create_sequence_from([spec]).unwrap();
        assert!(add_fn(&lua).call::<()>(specs).is_err());

        let specs = lua
            .create_sequence_from(["https://github.com/user/repo"])
            .unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("typo", true).unwrap();
        assert!(add_fn(&lua).call::<()>((specs, opts)).is_err());
        assert!(store.lock().unwrap().specs.is_empty());
    }

    #[test]
    fn a_builtin_owner_name_is_rejected() {
        let (lua, _store) = lua_with_store();
        let spec = lua.create_table().unwrap();
        spec.set("src", "https://github.com/user/repo").unwrap();
        spec.set("name", "bash").unwrap();
        let specs = lua.create_sequence_from([spec]).unwrap();

        let error = add_fn(&lua).call::<()>(specs).unwrap_err().to_string();

        assert!(error.contains("builtin plugin"));
    }

    /// A source whose name cannot be derived has to be named explicitly rather
    /// than installed under an empty directory name.
    #[test]
    fn a_source_with_no_derivable_name_is_refused() {
        let (lua, _store) = lua_with_store();
        let specs = lua.create_sequence_from(["///"]).unwrap();
        assert!(add_fn(&lua).call::<()>(specs).is_err());
    }

    #[test]
    fn the_first_declaration_of_a_name_wins() {
        let (lua, store) = lua_with_store();
        let first = lua.create_table().unwrap();
        first.set("src", "https://github.com/user/a").unwrap();
        first.set("name", "pkg").unwrap();
        let second = lua.create_table().unwrap();
        second.set("src", "https://github.com/user/b").unwrap();
        second.set("name", "pkg").unwrap();

        add_fn(&lua)
            .call::<()>(lua.create_sequence_from([first]).unwrap())
            .unwrap();
        add_fn(&lua)
            .call::<()>(lua.create_sequence_from([second]).unwrap())
            .unwrap();

        let declared = store.lock().unwrap();
        assert_eq!(declared.specs.len(), 1);
        assert_eq!(declared.specs[0].spec.src, "https://github.com/user/a");
    }

    fn call(lua: &Lua, name: &str, arg: Table) -> mlua::Result<()> {
        let f: mlua::Function = create_pack_table(lua).unwrap().get(name).unwrap();
        f.call(arg)
    }

    /// Mutating calls record work and return. Doing it inline would unload an
    /// owner, and unloading waits on the runtime thread the call occupies.
    #[test]
    fn update_and_del_record_work_rather_than_doing_it() {
        let (lua, store) = lua_with_store();
        call(&lua, "update", lua.create_sequence_from(["a"]).unwrap()).unwrap();
        call(&lua, "del", lua.create_sequence_from(["b"]).unwrap()).unwrap();

        let pending = store.lock().unwrap().pending.clone();
        assert_eq!(
            pending,
            vec![
                PackOp::Update {
                    name: "a".to_owned(),
                    options: UpdateOptions::default(),
                },
                PackOp::Delete {
                    name: "b".to_owned()
                },
            ]
        );
    }

    /// `packadd` is what makes an `opt/` package reachable at all, so it must
    /// register on the root table and queue an activation.
    #[test]
    fn packadd_registers_on_the_root_table_and_queues_activation() {
        let (lua, store) = lua_with_store();
        let maki = lua.create_table().unwrap();
        add_packadd(&lua, &maki).unwrap();

        let f: mlua::Function = maki.get("packadd").expect("packadd should be registered");
        f.call::<()>("demo").unwrap();

        assert_eq!(
            store.lock().unwrap().pending,
            vec![PackOp::Activate {
                name: "demo".to_owned()
            }]
        );
    }

    #[test]
    fn packadd_refuses_an_unsafe_name() {
        let (lua, store) = lua_with_store();
        let maki = lua.create_table().unwrap();
        add_packadd(&lua, &maki).unwrap();
        let f: mlua::Function = maki.get("packadd").unwrap();

        assert!(f.call::<()>("../escape").is_err());
        assert!(store.lock().unwrap().pending.is_empty());
    }

    /// Omitting `load` still loads the package, at the startup phase rather
    /// than inside the call, which is what Neovim's default defers to.
    #[test]
    fn a_declaration_without_load_is_eager() {
        let (lua, store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://github.com/user/repo"])
            .unwrap();
        add_fn(&lua).call::<()>(specs).unwrap();
        assert_eq!(store.lock().unwrap().specs[0].load, LoadMode::Eager);
    }

    /// `load = false` is what a lazy trigger will later delay, so it has to be
    /// a real dormant state rather than a synonym for `true`.
    #[test]
    fn load_false_leaves_the_package_dormant() {
        let (lua, store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://github.com/user/repo"])
            .unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("load", false).unwrap();

        let f: mlua::Function = create_pack_table(&lua).unwrap().get("add").unwrap();
        f.call::<()>((specs, opts)).unwrap();
        assert_eq!(store.lock().unwrap().specs[0].load, LoadMode::Dormant);
    }

    #[test]
    fn a_trigger_table_records_its_triggers() {
        let (lua, store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://github.com/user/repo"])
            .unwrap();
        let load = lua.create_table().unwrap();
        load.set("event", lua.create_sequence_from(["TurnStart"]).unwrap())
            .unwrap();
        load.set("cmd", "/demo").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("load", load).unwrap();

        let f: mlua::Function = create_pack_table(&lua).unwrap().get("add").unwrap();
        f.call::<()>((specs, opts)).unwrap();

        let declared = store.lock().unwrap();
        let LoadMode::Triggered(triggers) = &declared.specs[0].load else {
            panic!(
                "expected a triggered load, got {:?}",
                declared.specs[0].load
            );
        };
        assert_eq!(triggers.event, vec!["TurnStart".to_owned()]);
        assert_eq!(
            triggers.cmd,
            vec!["/demo".to_owned()],
            "a bare string counts"
        );
        assert!(triggers.keys.is_empty());
    }

    /// A table naming no trigger leaves the package dormant with nothing able
    /// to wake it, which is a typo rather than an intent.
    #[test]
    fn an_empty_trigger_table_is_rejected() {
        let (lua, _store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://github.com/user/repo"])
            .unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("load", lua.create_table().unwrap()).unwrap();

        let f: mlua::Function = create_pack_table(&lua).unwrap().get("add").unwrap();
        assert!(f.call::<()>((specs, opts)).is_err());
    }

    #[test]
    fn a_non_boolean_load_is_rejected() {
        let (lua, _store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://github.com/user/repo"])
            .unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("load", 7).unwrap();

        let f: mlua::Function = create_pack_table(&lua).unwrap().get("add").unwrap();
        assert!(f.call::<()>((specs, opts)).is_err());
    }

    #[test]
    fn the_same_operation_is_not_queued_twice() {
        let (lua, store) = lua_with_store();
        for _ in 0..3 {
            call(&lua, "update", lua.create_sequence_from(["a"]).unwrap()).unwrap();
        }
        assert_eq!(store.lock().unwrap().pending.len(), 1);
    }

    /// A name reaches the filesystem, so it is refused here rather than later.
    #[test]
    fn an_unsafe_name_is_refused_by_update_and_del() {
        let (lua, store) = lua_with_store();
        for bad in ["../escape", "/etc"] {
            assert!(call(&lua, "update", lua.create_sequence_from([bad]).unwrap()).is_err());
            assert!(call(&lua, "del", lua.create_sequence_from([bad]).unwrap()).is_err());
        }
        assert!(store.lock().unwrap().pending.is_empty());
    }

    /// Deleting nothing is a mistake worth reporting, unlike updating
    /// everything, which is what Neovim's omitted names mean.
    #[test]
    fn del_requires_a_name() {
        let (lua, _store) = lua_with_store();
        assert!(call(&lua, "del", lua.create_table().unwrap()).is_err());
    }

    #[test]
    fn declaration_order_is_preserved() {
        let (lua, store) = lua_with_store();
        let specs = lua
            .create_sequence_from([
                "https://github.com/user/first",
                "https://github.com/user/second",
            ])
            .unwrap();
        add_fn(&lua).call::<()>(specs).unwrap();

        let declared = store.lock().unwrap();
        let names: Vec<&str> = declared
            .specs
            .iter()
            .map(|d| d.spec.name.as_str())
            .collect();
        assert_eq!(names, vec!["first", "second"]);
    }
}
