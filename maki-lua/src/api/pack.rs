//! `maki.pack`, modelled after Neovim's `vim.pack`.
//!
//! Declaring a package records it and returns. Nothing is cloned or loaded from
//! inside the call, because `add` runs while `init.lua` is being sourced and
//! every load path blocks on a reply from the same runtime thread the caller is
//! occupying. The host installs and loads the recorded set afterwards, which is
//! also the phase Neovim defers to.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use maki_lua_macro::{lua_fn, lua_table};

use crate::api::options::PluginOpts;
use crate::plugin_permissions::PluginPermissions;
use maki_pack::{Spec, Version};
use mlua::{Lua, MultiValue, RegistryKey, Result as LuaResult, Table, Value as LuaValue};

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
    /// The function supplied by `opts.load` is fully responsible for loading.
    ///
    /// It carries the function rather than sitting beside an `Option` field,
    /// so "custom with no loader" cannot be built and no caller has to handle
    /// a case that has no meaning.
    Custom(Arc<RegistryKey>),
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
    /// Slash command names, always stored with the leading slash so they match
    /// what `register_command` records.
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
    pub confirm: bool,
    /// Opaque Lua data retained in the runtime registry.
    pub data: Option<Arc<RegistryKey>>,
}

impl Declared {
    /// Whether this entry point must load the package during startup.
    pub fn loads_at_start(&self, delivers_agent_events: bool) -> bool {
        matches!(self.load, LoadMode::Eager | LoadMode::Custom(_))
            || (!delivers_agent_events
                && matches!(&self.load, LoadMode::Triggered(t) if !t.event.is_empty()))
    }
}

/// Everything `init.lua` declared, in declaration order.
///
/// Session state only. What is installed, and at which revision, lives in the
/// lockfile, and this never holds a second opinion about it.
#[derive(Debug, Default, Clone)]
pub struct PackDeclarations {
    pub specs: Vec<Declared>,
    /// Package owners that loaded successfully in this runtime.
    pub active: BTreeSet<String>,
    /// Operations recorded by Lua for the host to perform after the calling
    /// task has exited. Nothing here runs inline: `update` and `del` unload an
    /// owner, and unloading blocks on a reply from the runtime thread the
    /// caller is occupying.
    pub pending: Vec<PackOp>,
    /// Packages installed but not loaded, with what should wake them.
    ///
    /// `None` means startup has not built the activation catalog yet, so
    /// `maki.packadd` must record a host operation. `Some` means the runtime
    /// owns activation, including when the catalog is empty.
    pub dormant: Option<Vec<Dormant>>,
    /// Set once the drain point has taken the pending operations.
    ///
    /// Arming the catalog can fail, and then `dormant` stays `None` for the
    /// rest of the session and every `maki.packadd` records an operation that
    /// nothing will ever read. This says the queue is closed, whatever the
    /// catalog did.
    pub drained: bool,
}

/// One installed, unloaded package and the triggers that activate it.
#[derive(Debug, Clone)]
pub struct Dormant {
    pub name: String,
    /// Where the package was installed. The runtime reads its entrypoints from
    /// here when a trigger fires.
    pub dir: PathBuf,
    /// Already narrowed by origin and approval. Stored rather than recomputed,
    /// so activation cannot accidentally grant more than a startup load would.
    pub permissions: PluginPermissions,
    pub opts: PluginOpts,
    pub triggers: Triggers,
    pub state: PackState,
}

/// Whether activation may still be attempted for a dormant package.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PackState {
    #[default]
    Inactive,
    /// A load that failed is not retried. Retrying on every keystroke would
    /// run a broken package's top level over and over.
    Failed(String),
}

impl Dormant {
    /// Whether this package answers to a command name.
    pub fn handles_command(&self, command: &str) -> bool {
        self.triggers.cmd.iter().any(|c| c == command)
    }

    /// Whether a trigger may still start a load.
    pub fn is_startable(&self) -> bool {
        matches!(self.state, PackState::Inactive)
    }
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
    /// Skip update review. Permission approval remains separate.
    pub force: bool,
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
    Delete { name: String, force: bool },
    /// Load a package that is installed but dormant.
    Activate { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackChangeKind {
    Install,
    Update,
    Delete,
}

impl PackChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PackChange {
    pub declared: Declared,
    pub active: bool,
    pub kind: PackChangeKind,
    pub path: PathBuf,
}

pub type PackStore = Arc<Mutex<PackDeclarations>>;

#[derive(Clone)]
pub(crate) struct PackRuntimeTx(pub flume::Sender<crate::runtime::Request>);

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
    // A package name is an owner name, and an owner name is what unloading
    // takes. Letting a package be called `bash` would make `pack.del("bash")`
    // tear down the bundled bash tool, its keymaps, and its hints, so the name
    // is refused here rather than at the point of damage. Manual discovery
    // already refuses the same names.
    if crate::loader::is_bundled(&spec.name) {
        return Err(mlua::Error::runtime(format!(
            "pack.add: {:?} is the name of a builtin plugin; set 'name' to something else",
            spec.name
        )));
    }
    Ok(spec)
}

/// Reads a spec entry, which is either a string source or a table.
fn parse_spec(lua: &Lua, value: LuaValue) -> LuaResult<(Spec, Option<Arc<RegistryKey>>)> {
    let table = match value {
        // Neovim accepts a bare string and treats it as `src`. It still goes
        // through the name check below: a source whose name cannot be derived
        // is no safer for having been written as a string.
        LuaValue::String(s) => {
            return finish(Spec::new(s.to_str()?.to_owned())).map(|spec| (spec, None));
        }
        LuaValue::Table(t) => t,
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.add: each spec must be a string or a table, got {}",
                other.type_name()
            )));
        }
    };
    reject_unknown_fields(&table, &["src", "name", "version", "data"], "pack.add spec")?;

    let src = match table.get::<LuaValue>("src")? {
        LuaValue::Nil => {
            return Err(mlua::Error::runtime("pack.add: spec is missing 'src'"));
        }
        LuaValue::String(src) => src.to_str()?.to_owned(),
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
            LuaValue::String(version) => {
                let version = version.to_str()?.to_owned();
                if version.trim().is_empty() {
                    return Err(mlua::Error::runtime(
                        "pack.add: 'version' must not be empty",
                    ));
                }
                spec.with_version(Version::Rev(version))
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

    let data = match table.get::<LuaValue>("data")? {
        LuaValue::Nil => None,
        value => Some(Arc::new(lua.create_registry_value(value)?)),
    };

    finish(spec).map(|spec| (spec, data))
}

/// Declare packages to install and load, like `vim.pack.add`.
///
/// Recording is all this does. The host installs and loads the declared set
/// after `init.lua` finishes, so a slow clone never blocks the call and a
/// package failure never stops maki from starting.
/// The first declaration of a package name wins for the current session.
///
/// Only available inside the global `init.lua`. Declaring a package fetches
/// code and changes state shared by every project.
///
/// @param specs table List of specs. Each is a source string, or a table with
///   `src` (string, required), `name` (string), `version` (string or
///   `maki.version.range()`), and arbitrary `data` passed to `get`, change
///   events, and a custom loader.
/// @param opts table? Options: `confirm` (boolean, default true) asks before
///   installation. `load` (boolean|table|function) controls loading. `false`
///   leaves the package for `maki.packadd`. A table of `event`, `cmd`, and
///   `keys` names what wakes it. A function runs as the package owner with
///   `{ spec, path }` and is fully responsible for loading it. Credentials in
///   `spec.src` are redacted. The first
///   trigger loads the package and is then delivered to it. `event` accepts
///   only the host events that `create_autocmd` documents.
///   HTTP sources with credentials are rejected; use a Git credential helper.
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
    for declared in parsed {
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
        LuaValue::Table(t) => parse_triggers(&t)?,
        LuaValue::Function(function) => {
            LoadMode::Custom(Arc::new(lua.create_registry_value(function)?))
        }
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.add: 'load' must be a boolean, trigger table, or function, got {}",
                other.type_name()
            )));
        }
    };
    Ok((load, confirm))
}

/// Command names are stored the way `register_command` stores them.
///
/// That call adds a leading slash if the name has none. A trigger recorded
/// without one would publish `foo`, load the package, and then look for a
/// handler registered as `/foo`, so the command that woke the package would
/// not run.
fn normalize_command(name: &str) -> LuaResult<String> {
    let bare = name.strip_prefix('/').unwrap_or(name);
    if bare.is_empty() || name.chars().any(char::is_whitespace) || name.ends_with('!') {
        return Err(mlua::Error::runtime(format!(
            "pack.add: invalid command trigger {name:?}"
        )));
    }
    if name.starts_with('/') {
        Ok(name.to_owned())
    } else {
        Ok(format!("/{name}"))
    }
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
    for cmd in &mut triggers.cmd {
        *cmd = normalize_command(cmd)?;
    }
    for event in &triggers.event {
        if !crate::api::autocmd::LAZY_EVENTS.contains(&event.as_str()) {
            return Err(mlua::Error::runtime(format!(
                "pack.add: {event:?} is not an event fired by the host"
            )));
        }
    }
    for key in &triggers.keys {
        crate::api::keymap::parse_key_notation(key).map_err(|error| {
            mlua::Error::runtime(format!("pack.add: invalid key trigger {key:?}: {error}"))
        })?;
    }
    // An empty table would leave the package dormant with no way to wake it.
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
    if declarations.drained {
        return Err(mlua::Error::runtime(
            "maki.packadd: packages have already been loaded, so it only works \
             while init.lua and the packages themselves are running",
        ));
    }
    // Two calls naming one package still load it once. The name is checked
    // against what discovery found when the host drains this, so an unknown
    // one is reported there rather than guessed at here.
    if !declarations.pending.contains(&op) {
        declarations.pending.push(op);
    }
    Ok(())
}

/// Gets managed package information, optionally filtered by name.
///
/// This is the read-only part of `maki.pack`, so packages may call it too.
/// Information comes from the current declarations and lockfile and does not
/// contact a remote.
///
/// @param names table? Package names. Omit for all managed packages.
/// @param opts table? Reserved for future compatibility. Omit it.
/// @return (table) Package records with `spec`, `path`, `rev`, and `active`.
/// @example
/// local packages = maki.pack.get({ "maki-goal" })
#[lua_fn]
fn get(lua: &Lua, names: Option<Table>, opts: Option<Table>) -> LuaResult<Table> {
    validate_get_options(opts)?;
    let names = names
        .map(|table| table.sequence_values::<String>().collect())
        .transpose()?;
    let store = lua
        .app_data_ref::<PackStore>()
        .ok_or_else(|| mlua::Error::runtime("pack.get: not available here"))?
        .clone();
    let declarations = store.lock().expect("pack declarations").clone();
    let lock = crate::pack::read_lockfile(crate::pack::lockfile_path().as_deref())
        .ok_or_else(|| mlua::Error::runtime("pack.get: pack lockfile is unreadable"))?;

    let order = match names {
        Some(names) => names,
        None => {
            let mut names: Vec<String> = declarations
                .specs
                .iter()
                .map(|declared| declared.spec.name.clone())
                .collect();
            let extras: Vec<String> = lock
                .install_order()
                .filter(|name| !names.iter().any(|known| known == name))
                .map(str::to_owned)
                .collect();
            names.extend(extras);
            names
        }
    };

    // A missing data directory only means no path can be reported for a
    // record, which `path = nil` already says, so this reads as absence here.
    let site = crate::pack::site_dir().ok();
    let manager = site.as_ref().map(maki_pack::manager::Manager::new);
    let result = lua.create_table()?;
    for (index, name) in order.into_iter().enumerate() {
        let declared = declarations
            .specs
            .iter()
            .find(|declared| declared.spec.name == name);
        let locked = lock.get(&name);
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
        let path = manager
            .as_ref()
            .and_then(|manager| manager.resolve(&lock, &name));
        let item = lua.create_table()?;
        item.set(
            "spec",
            spec_to_lua(lua, spec, declared.and_then(|d| d.data.as_ref()))?,
        )?;
        item.set("path", path.map(|path| path.display().to_string()))?;
        item.set("rev", locked.map(|entry| entry.rev.as_str()))?;
        item.set("active", declarations.active.contains(&name))?;
        result.set(index + 1, item)?;
    }
    Ok(result)
}

fn validate_get_options(opts: Option<Table>) -> LuaResult<()> {
    let Some(opts) = opts else {
        return Ok(());
    };
    reject_unknown_fields(&opts, &[], "pack.get")
}

pub(crate) fn spec_to_lua(
    lua: &Lua,
    spec: &Spec,
    data: Option<&Arc<RegistryKey>>,
) -> LuaResult<Table> {
    let table = lua.create_table()?;
    table.set("src", maki_pack::git::redact(&spec.src))?;
    table.set("name", spec.name.as_str())?;
    match &spec.version {
        Version::DefaultBranch => {}
        Version::Rev(version) => table.set("version", version.as_str())?,
        Version::Range(range) => {
            let version = lua.create_table()?;
            version.set(super::version::RANGE_MARKER, range.to_string())?;
            table.set("version", version)?;
        }
    }
    if let Some(data) = data {
        table.set("data", lua.registry_value::<LuaValue>(data.as_ref())?)?;
    }
    Ok(table)
}

pub(crate) fn create_pack_read_table(lua: &Lua) -> LuaResult<Table> {
    let table = lua.create_table()?;
    table.set(
        "get",
        lua.create_function(|lua, (names, opts): (Option<Table>, Option<Table>)| {
            get(lua, names, opts)
        })?,
    )?;
    Ok(table)
}

pub(crate) fn restrict_management(lua: &Lua, table: &Table) -> LuaResult<()> {
    for method in ["add", "update", "del"] {
        table.set(
            method,
            lua.create_function(move |_, _: MultiValue| -> LuaResult<()> {
                Err(mlua::Error::runtime(format!(
                    "maki.pack.{method} is only available in the global init.lua"
                )))
            })?,
        )?;
    }
    Ok(())
}

/// Update packages to what their version now resolves to, like
/// `vim.pack.update`.
///
/// The work happens after this call returns, because updating unloads and
/// reloads the package and unloading waits on the runtime this call occupies.
/// Only available inside the global `init.lua`.
///
/// @param names table? Package names. Omit for every declared package.
/// @param opts table? `offline` works without the network. `target` is
///   `"version"` (the default) to take what version now resolves to, or
///   `"lockfile"` to go back to the recorded revision. `force` skips the
///   update review, but not a new permission approval.
/// @example
/// maki.pack.update({ "maki-goal" })
/// maki.pack.update(nil, { target = "lockfile" })
#[lua_fn]
fn update(lua: &Lua, names: Option<Table>, opts: Option<Table>) -> LuaResult<()> {
    let options = parse_update_options(opts)?;
    // Only an omitted argument means every package. An explicit empty list is
    // useful when callers compute a selection and must not become a bulk
    // update by accident.
    let names = match names {
        Some(names) => names
            .sequence_values::<String>()
            .collect::<LuaResult<_>>()?,
        None => declared_names(lua)?,
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
    reject_unknown_fields(&opts, &["offline", "force", "target"], "pack.update")?;
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
    let force = match opts.get::<LuaValue>("force")? {
        LuaValue::Nil => false,
        LuaValue::Boolean(force) => force,
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.update: 'force' must be a boolean, got {}",
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
    Ok(UpdateOptions {
        target,
        offline,
        force,
    })
}

/// Remove packages, like `vim.pack.del`.
///
/// Runs after this call returns, for the same reason as `update`.
/// Only available inside the global `init.lua`.
///
/// @param names table Package names to remove.
/// @param opts table? `force` allows removal of an active package.
/// @example
/// maki.pack.del({ "maki-goal" })
#[lua_fn]
fn del(lua: &Lua, names: Table, opts: Option<Table>) -> LuaResult<()> {
    let names: Vec<String> = names
        .sequence_values::<String>()
        .collect::<LuaResult<_>>()?;
    if names.is_empty() {
        return Err(mlua::Error::runtime("pack.del: name a package to remove"));
    }
    if let Some(opts) = &opts {
        reject_unknown_fields(opts, &["force"], "pack.del")?;
    }
    let force = match opts
        .as_ref()
        .map(|opts| opts.get::<LuaValue>("force"))
        .transpose()?
        .unwrap_or(LuaValue::Nil)
    {
        LuaValue::Nil => false,
        LuaValue::Boolean(force) => force,
        other => {
            return Err(mlua::Error::runtime(format!(
                "pack.del: 'force' must be a boolean, got {}",
                other.type_name()
            )));
        }
    };
    for name in names {
        enqueue(
            lua,
            PackOp::Delete {
                name: name.clone(),
                force,
            },
            &name,
        )?;
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
/// A runtime call reports an unknown, disabled, or previously failed package
/// immediately. During `init.lua`, the host reports the result after startup
/// has installed and cataloged packages.
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
    let runtime_tx = lua
        .app_data_ref::<PackRuntimeTx>()
        .map(|runtime| runtime.0.clone());
    let function = match runtime_tx {
        Some(tx) => {
            let store = lua
                .app_data_ref::<PackStore>()
                .ok_or_else(|| mlua::Error::runtime("packadd: not available here"))?
                .clone();
            lua.create_function(move |_, name: String| {
                if !maki_pack::name_is_safe(&name) {
                    return Err(mlua::Error::runtime(format!(
                        "pack: {name:?} is not a usable package name"
                    )));
                }
                let state = {
                    let mut declarations = store.lock().expect("pack declarations");
                    let Some(dormant) = declarations.dormant.as_ref() else {
                        if declarations.drained {
                            return Err(mlua::Error::runtime(format!(
                                "packadd: package {name:?} cannot be activated, because \
                                 startup could not build the activation catalog"
                            )));
                        }
                        let operation = PackOp::Activate { name };
                        if !declarations.pending.contains(&operation) {
                            declarations.pending.push(operation);
                        }
                        return Ok(());
                    };
                    if declarations.active.contains(&name) {
                        return Ok(());
                    }
                    dormant
                        .iter()
                        .find(|package| package.name == name)
                        .map(|package| package.state.clone())
                };
                match state {
                    Some(PackState::Inactive) => {}
                    Some(PackState::Failed(error)) => {
                        return Err(mlua::Error::runtime(format!(
                            "packadd: package {name:?} already failed to load: {error}"
                        )));
                    }
                    None => {
                        return Err(mlua::Error::runtime(format!(
                            "packadd: package {name:?} is not installed or is disabled"
                        )));
                    }
                }
                tx.send(crate::runtime::Request::ActivatePackage { name })
                    .map_err(|_| mlua::Error::runtime("packadd: plugin host is not running"))
            })?
        }
        None => lua.create_function(packadd)?,
    };
    maki.set("packadd", function)
}

lua_table! {
    /// Manage global external packages, modelled after `vim.pack`.
    ///
    /// `get` is read-only and available to project config and plugins.
    /// `add`, `update`, and `del` are available only inside the global
    /// `init.lua`, because they change state shared by every project.
    ///
    /// ```lua
    /// maki.pack.add({ "https://github.com/user/maki-goal" })
    /// ```
    "maki.pack" => pub(crate) fn create_pack_table(), DOCS [
        add, get, update, del,
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

    #[test]
    fn get_rejects_options_that_it_does_not_implement() {
        let lua = Lua::new();
        let opts = lua.create_table().unwrap();
        opts.set("offline", true).unwrap();

        let error = validate_get_options(Some(opts)).unwrap_err();

        assert!(error.to_string().contains("unknown field"), "{error}");
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
    fn arbitrary_data_round_trips_without_serialization() {
        let (lua, store) = lua_with_store();
        let data = lua.create_table().unwrap();
        data.set("number", 7).unwrap();
        data.set("call", lua.create_function(|_, ()| Ok("ok")).unwrap())
            .unwrap();
        let spec = lua.create_table().unwrap();
        spec.set("src", "https://github.com/user/repo").unwrap();
        spec.set("data", data).unwrap();
        add_fn(&lua)
            .call::<()>(lua.create_sequence_from([spec]).unwrap())
            .unwrap();

        let declared = store.lock().unwrap().specs[0].clone();
        let output = spec_to_lua(&lua, &declared.spec, declared.data.as_ref()).unwrap();
        let output_data: Table = output.get("data").unwrap();
        let call: mlua::Function = output_data.get("call").unwrap();
        assert_eq!(output_data.get::<i64>("number").unwrap(), 7);
        assert_eq!(call.call::<String>(()).unwrap(), "ok");
    }

    #[test]
    fn http_credentials_are_rejected_without_exposing_them() {
        let (lua, _store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://user:secret@example.com/repo\u{1b}"])
            .unwrap();

        let error = add_fn(&lua).call::<()>(specs).unwrap_err().to_string();

        assert!(error.contains("credential helper"), "got: {error}");
        assert!(!error.contains("secret"), "credential leaked: {error}");
        assert!(
            !error.contains('\u{1b}'),
            "control character leaked: {error}"
        );
    }

    #[test]
    fn an_empty_add_does_not_change_later_confirmation() {
        let (lua, store) = lua_with_store();
        let opts = lua.create_table().unwrap();
        opts.set("confirm", false).unwrap();
        add_fn(&lua)
            .call::<()>((lua.create_table().unwrap(), opts))
            .unwrap();
        add_fn(&lua)
            .call::<()>(
                lua.create_sequence_from(["https://github.com/user/repo"])
                    .unwrap(),
            )
            .unwrap();

        let declarations = store.lock().unwrap();
        assert!(declarations.specs[0].confirm);
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

    #[test]
    fn a_non_string_source_reports_its_type() {
        let (lua, _store) = lua_with_store();
        let spec = lua.create_table().unwrap();
        spec.set("src", 7).unwrap();
        let specs = lua.create_sequence_from([spec]).unwrap();

        let error = add_fn(&lua).call::<()>(specs).unwrap_err();

        assert!(error.to_string().contains("must be a string"), "{error}");
    }

    #[test]
    fn a_misspelled_spec_field_is_rejected() {
        let (lua, _store) = lua_with_store();
        let spec = lua.create_table().unwrap();
        spec.set("src", "https://github.com/user/repo").unwrap();
        spec.set("verison", "v1").unwrap();
        let specs = lua.create_sequence_from([spec]).unwrap();

        let error = add_fn(&lua).call::<()>(specs).unwrap_err();

        assert!(error.to_string().contains("verison"), "{error}");
    }

    #[test]
    fn an_empty_version_is_rejected() {
        let (lua, _store) = lua_with_store();
        let spec = lua.create_table().unwrap();
        spec.set("src", "https://github.com/user/repo").unwrap();
        spec.set("version", " ").unwrap();
        let specs = lua.create_sequence_from([spec]).unwrap();

        assert!(add_fn(&lua).call::<()>(specs).is_err());
    }

    #[test]
    fn an_empty_or_non_string_explicit_name_is_rejected() {
        let (lua, _store) = lua_with_store();
        for name in [
            LuaValue::String(lua.create_string("").unwrap()),
            LuaValue::Integer(7),
        ] {
            let spec = lua.create_table().unwrap();
            spec.set("src", "https://github.com/user/repo").unwrap();
            spec.set("name", name).unwrap();
            let specs = lua.create_sequence_from([spec]).unwrap();

            assert!(add_fn(&lua).call::<()>(specs).is_err());
        }
    }

    /// A package name is an owner name. If a package could be called `bash`,
    /// removing it would unload the bundled bash tool rather than the package.
    #[test]
    fn a_builtin_name_is_refused() {
        let lua = Lua::new();
        lua.set_app_data(PackStore::default());
        let spec = lua.create_table().unwrap();
        spec.set("src", "https://example.com/x").unwrap();
        spec.set("name", "bash").unwrap();
        let specs = lua.create_sequence_from([spec]).unwrap();

        let err = call(&lua, "add", specs).expect_err("a builtin name is not available");
        assert!(err.to_string().contains("builtin"), "{err}");
    }

    /// The name becomes a directory, so reject it before filesystem access.
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

    /// A source whose name cannot be derived has to be named explicitly rather
    /// than installed under an empty directory name.
    #[test]
    fn a_source_with_no_derivable_name_is_refused() {
        let (lua, _store) = lua_with_store();
        let specs = lua.create_sequence_from(["///"]).unwrap();
        assert!(add_fn(&lua).call::<()>(specs).is_err());
    }

    #[test]
    fn the_first_declaration_of_a_package_wins() {
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
                    name: "b".to_owned(),
                    force: false,
                },
            ]
        );
    }

    #[test]
    fn an_explicit_empty_update_list_does_not_update_everything() {
        let (lua, store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://github.com/user/a"])
            .unwrap();
        add_fn(&lua).call::<()>(specs).unwrap();

        call(&lua, "update", lua.create_table().unwrap()).unwrap();

        assert!(store.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn update_and_delete_force_options_reach_the_pending_operations() {
        let (lua, store) = lua_with_store();
        let options = lua.create_table().unwrap();
        options.set("force", true).unwrap();
        let update: mlua::Function = create_pack_table(&lua).unwrap().get("update").unwrap();
        update
            .call::<()>((lua.create_sequence_from(["a"]).unwrap(), options.clone()))
            .unwrap();
        let delete: mlua::Function = create_pack_table(&lua).unwrap().get("del").unwrap();
        delete
            .call::<()>((lua.create_sequence_from(["b"]).unwrap(), options))
            .unwrap();

        assert!(matches!(
            &store.lock().unwrap().pending[..],
            [
                PackOp::Update {
                    options: UpdateOptions { force: true, .. },
                    ..
                },
                PackOp::Delete { force: true, .. }
            ]
        ));
    }

    #[test]
    fn update_and_delete_reject_unknown_options() {
        let (lua, store) = lua_with_store();
        for name in ["update", "del"] {
            let options = lua.create_table().unwrap();
            options.set("focre", true).unwrap();
            let function: mlua::Function = create_pack_table(&lua).unwrap().get(name).unwrap();

            let error = function
                .call::<()>((lua.create_sequence_from(["a"]).unwrap(), options))
                .unwrap_err();

            assert!(error.to_string().contains("focre"), "{error}");
        }
        assert!(store.lock().unwrap().pending.is_empty());
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
    fn startup_packadd_queues_one_activation_per_package() {
        let (lua, store) = lua_with_store();
        let maki = lua.create_table().unwrap();
        add_packadd(&lua, &maki).unwrap();
        let function: mlua::Function = maki.get("packadd").unwrap();

        function.call::<()>("demo").unwrap();
        function.call::<()>("demo").unwrap();

        assert_eq!(store.lock().unwrap().pending.len(), 1);
    }

    #[test]
    fn runtime_packadd_queues_once_before_the_catalog_is_ready() {
        let (lua, store) = lua_with_store();
        let maki = lua.create_table().unwrap();
        let (tx, _rx) = flume::unbounded();
        lua.set_app_data(PackRuntimeTx(tx));
        add_packadd(&lua, &maki).unwrap();
        let function: mlua::Function = maki.get("packadd").unwrap();

        function.call::<()>("demo").unwrap();
        function.call::<()>("demo").unwrap();

        assert_eq!(store.lock().unwrap().pending.len(), 1);
    }

    /// The recording path is only read by the startup drain. Arming the
    /// activation catalog can fail, and then every later `packadd` lands here,
    /// so it has to report rather than queue where nothing will look.
    #[test]
    fn packadd_after_the_drain_is_refused() {
        let (lua, store) = lua_with_store();
        let maki = lua.create_table().unwrap();
        add_packadd(&lua, &maki).unwrap();
        store.lock().unwrap().drained = true;

        let f: mlua::Function = maki.get("packadd").expect("packadd should be registered");
        let err = f
            .call::<()>("demo")
            .expect_err("nothing reads the queue after the drain");
        assert!(
            err.to_string().contains("already been loaded"),
            "got: {err}"
        );
        assert!(
            store.lock().unwrap().pending.is_empty(),
            "a refused call must not leave a request behind"
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
    fn misspelled_load_fields_are_rejected() {
        let (lua, _store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://github.com/user/repo"])
            .unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("lod", false).unwrap();

        let error = add_fn(&lua).call::<()>((specs, opts)).unwrap_err();

        assert!(error.to_string().contains("lod"), "{error}");
    }

    #[test]
    fn a_custom_loader_is_retained_and_runs_at_start() {
        let (lua, store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://github.com/user/repo"])
            .unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("load", lua.create_function(|_, ()| Ok(())).unwrap())
            .unwrap();

        add_fn(&lua).call::<()>((specs, opts)).unwrap();

        let declared = store.lock().unwrap().specs[0].clone();
        assert!(matches!(declared.load, LoadMode::Custom(_)));
        assert!(declared.loads_at_start(true));
    }

    #[test]
    fn a_trigger_table_records_its_triggers() {
        let (lua, store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://github.com/user/repo"])
            .unwrap();
        let load = lua.create_table().unwrap();
        load.set(
            "event",
            lua.create_sequence_from(["TurnStart", "TaskStatusChanged"])
                .unwrap(),
        )
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
        assert_eq!(
            triggers.event,
            vec!["TurnStart".to_owned(), "TaskStatusChanged".to_owned()]
        );
        assert_eq!(
            triggers.cmd,
            vec!["/demo".to_owned()],
            "a bare string counts"
        );
        assert!(triggers.keys.is_empty());
    }

    #[test]
    fn a_misspelled_trigger_field_is_rejected() {
        let (lua, _store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://github.com/user/repo"])
            .unwrap();
        let load = lua.create_table().unwrap();
        load.set("comand", "/demo").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("load", load).unwrap();

        let error = add_fn(&lua).call::<()>((specs, opts)).unwrap_err();

        assert!(error.to_string().contains("comand"), "{error}");
    }

    #[test]
    fn headless_start_promotes_event_triggers_but_not_command_triggers() {
        let event = Declared {
            spec: Spec::new("https://example.com/event"),
            load: LoadMode::Triggered(Triggers {
                event: vec!["TurnStart".to_owned()],
                ..Triggers::default()
            }),
            confirm: true,
            data: None,
        };
        let command = Declared {
            spec: Spec::new("https://example.com/command"),
            load: LoadMode::Triggered(Triggers {
                cmd: vec!["/demo".to_owned()],
                ..Triggers::default()
            }),
            confirm: true,
            data: None,
        };

        assert!(!event.loads_at_start(true));
        assert!(event.loads_at_start(false));
        assert!(!command.loads_at_start(false));
    }

    #[test]
    fn read_only_pack_table_exposes_only_get() {
        let lua = Lua::new();
        let table = create_pack_read_table(&lua).unwrap();
        assert!(table.contains_key("get").unwrap());
        assert!(!table.contains_key("add").unwrap());
        assert!(!table.contains_key("update").unwrap());
        assert!(!table.contains_key("del").unwrap());
    }

    /// `register_command` stores a leading slash whether or not the package
    /// wrote one, so a trigger recorded without one would wake the package and
    /// then fail to find the very command that woke it.
    #[test]
    fn a_command_trigger_is_stored_with_a_leading_slash() {
        let (lua, store) = lua_with_store();
        let opts = lua.create_table().unwrap();
        let load = lua.create_table().unwrap();
        load.set("cmd", "bare").unwrap();
        opts.set("load", load).unwrap();

        let specs = lua
            .create_sequence_from(["https://example.com/demo"])
            .unwrap();
        let f: mlua::Function = create_pack_table(&lua).unwrap().get("add").unwrap();
        f.call::<()>((specs, opts)).unwrap();

        let declared = store.lock().unwrap().specs[0].clone();
        match declared.load {
            LoadMode::Triggered(t) => assert_eq!(t.cmd, vec!["/bare".to_owned()]),
            other => panic!("expected a trigger, got {other:?}"),
        }
    }

    #[test]
    fn command_triggers_must_be_dispatchable_words() {
        for command in ["/", "two words", "/forced!"] {
            let (lua, _store) = lua_with_store();
            let opts = lua.create_table().unwrap();
            let load = lua.create_table().unwrap();
            load.set("cmd", command).unwrap();
            opts.set("load", load).unwrap();
            let specs = lua
                .create_sequence_from(["https://example.com/demo"])
                .unwrap();

            assert!(add_fn(&lua).call::<()>((specs, opts)).is_err(), "{command}");
        }
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
    fn events_without_an_activation_path_are_rejected() {
        for event in ["PluginOnly", "PackChanged"] {
            let (lua, _store) = lua_with_store();
            let specs = lua
                .create_sequence_from(["https://github.com/user/repo"])
                .unwrap();
            let load = lua.create_table().unwrap();
            load.set("event", event).unwrap();
            let opts = lua.create_table().unwrap();
            opts.set("load", load).unwrap();

            assert!(add_fn(&lua).call::<()>((specs, opts)).is_err(), "{event}");
        }
    }

    #[test]
    fn an_invalid_key_trigger_is_rejected() {
        let (lua, _store) = lua_with_store();
        let specs = lua
            .create_sequence_from(["https://github.com/user/repo"])
            .unwrap();
        let load = lua.create_table().unwrap();
        load.set("keys", "<Not-A-Key>").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("load", load).unwrap();

        assert!(add_fn(&lua).call::<()>((specs, opts)).is_err());
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
