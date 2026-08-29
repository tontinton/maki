use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use include_dir::{Dir, include_dir};
use maki_agent::tools::interpreter_bridge::build_tool_input;
use maki_agent::tools::{FileAccess, FileKey, grep as grep_tool};
use maki_agent::{LoadedInstructions, find_subdirectory_instructions, is_instruction_file};
use maki_lua::{json_to_lua, lua_to_json};
use mlua::MultiValue;
use mlua::prelude::*;
use mlua::{UserData, UserDataMethods};
use serde_json::Value;
use tracing::{debug, warn};

static EMBEDDED_PLUGINS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../plugins");

/// Global name of the serialized `AgentConfig` table, if the parent sent one.
const CONFIG_GLOBAL: &str = "_config";

/// Wire prefix for host-focused `maki.ui.*` calls the sandbox forwards to the
/// parent; the parent answers them from its own plugin Lua API.
pub const HOST_UI_PREFIX: &str = "maki.ui.";

/// Forward a call to a host-answered tool (the parent's plugin API rather
/// than a registered sandbox tool), mirroring `ChildCtx::forward_trusted`.
pub type HostForward =
    Arc<dyn Fn(&str, Vec<Value>, Vec<(String, Value)>) -> Result<String, String> + Send + Sync>;

/// Minimal Lua runtime for the sandbox child.
///
/// Provides a stripped-down `maki.*` API so the existing tool plugins
/// (read, write, edit, glob, grep) can load and execute inside the
/// mount namespace.  Filesystem ops are naturally sandboxed — they only
/// see paths mounted inside the namespace.
pub struct ChildLuaRuntime {
    lua: Lua,
    tracker: Arc<FileAccess>,
    instructions: LoadedInstructions,
}

impl ChildLuaRuntime {
    #[cfg(test)]
    pub fn new(plugin_dir: &Path) -> Result<Self, LuaError> {
        Self::build(plugin_dir, None)
    }

    /// Like [`Self::new`], but forwards host-side API calls (e.g. status
    /// hints) to the parent through `forward`.
    pub fn with_forwarder(plugin_dir: &Path, forward: HostForward) -> Result<Self, LuaError> {
        Self::build(plugin_dir, Some(forward))
    }

    fn build(plugin_dir: &Path, forward: Option<HostForward>) -> Result<Self, LuaError> {
        let lua = Lua::new();
        create_maki_api(&lua, forward)?;
        setup_require(&lua, plugin_dir.to_path_buf())?;
        load_plugins(&lua, plugin_dir)?;
        Ok(Self {
            lua,
            tracker: FileAccess::fresh(),
            instructions: LoadedInstructions::new(),
        })
    }

    /// Replace the serialized `AgentConfig` in the `_config` global.
    pub fn set_config(&self, config_json: &str) -> Result<(), String> {
        let value: Value =
            serde_json::from_str(config_json).map_err(|e| format!("config json: {e}"))?;
        let lua_value = json_to_lua(&self.lua, &value).map_err(|e| e.to_string())?;
        self.lua
            .globals()
            .set(CONFIG_GLOBAL, lua_value)
            .map_err(|e| e.to_string())
    }

    /// Execute a user `init.lua` after the embedded plugins have loaded, so
    /// custom tools register through the same `maki.api.register_tool` path.
    pub fn run_init_file(&self, path: &Path) -> Result<(), String> {
        let src =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        self.lua
            .load(&src)
            .set_name(path.to_string_lossy().to_string())
            .exec()
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Call a registered tool by name.
    pub fn call_tool(
        &self,
        name: &str,
        args: &[Value],
        kwargs: &[(String, Value)],
    ) -> Result<(String, bool), String> {
        let globals = self.lua.globals();
        let tools: LuaTable = globals
            .get("_registered_tools")
            .map_err(|e| format!("no registered tools: {e}"))?;
        let handler: LuaFunction = tools
            .get(name)
            .map_err(|e| format!("tool '{name}' not found: {e}"))?;

        let input = build_tool_input(args, kwargs).map_err(|e| format!("{name}: {e}"))?;
        let input_lua = json_to_lua(&self.lua, &input).map_err(|e| e.to_string())?;
        let ctx = build_ctx(&self.lua, &self.tracker, &self.instructions)
            .map_err(|e| format!("{name}: build ctx: {e}"))?;

        let values: LuaMultiValue = handler
            .call((input_lua, ctx))
            .map_err(|e| format!("{name}: {e}"))?;

        extract_tool_result(values)
    }

    /// List the names of all registered tools.
    pub fn registered_tool_names(&self) -> Result<Vec<String>, String> {
        let globals = self.lua.globals();
        let tools: LuaTable = globals
            .get("_registered_tools")
            .map_err(|e| format!("no registered tools: {e}"))?;
        let mut names = Vec::new();
        for pair in tools.pairs::<String, LuaFunction>() {
            let (name, _) = pair.map_err(|e| e.to_string())?;
            names.push(name);
        }
        Ok(names)
    }
}

// ──────────────────────────────────────────────
//  Tool ctx (input's second argument)
// ──────────────────────────────────────────────

fn config_table(lua: &Lua) -> LuaResult<LuaTable> {
    match lua
        .globals()
        .get::<LuaValue>(CONFIG_GLOBAL)
        .unwrap_or(LuaValue::Nil)
    {
        LuaValue::Table(t) => Ok(t),
        _ => lua.create_table(),
    }
}

/// Per-call ctx for the child: a userdata mirroring the subset of the
/// parent `LuaCtx` surface the filesystem tools use: `config`,
/// `tool_output_lines`, `record_read`, `check_before_edit`,
/// `is_instruction_file`, `find_instructions`.
struct ChildCtx {
    tracker: Arc<FileAccess>,
    instructions: LoadedInstructions,
}

impl UserData for ChildCtx {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("config", |lua, _this, args: MultiValue| {
            let key: Option<LuaResult<String>> = (0..args.len())
                .next()
                .map(|_| lua.from_value::<String>(args[0].clone()));
            let default = args.get(1).cloned().unwrap_or(LuaValue::Nil);
            match key {
                None => config_table(lua).map(LuaValue::Table),
                Some(Ok(key)) => {
                    let cfg = config_table(lua)?;
                    let val = cfg.raw_get::<LuaValue>(key.as_str())?;
                    Ok(if val.is_nil() { default } else { val })
                }
                Some(Err(e)) => Err(e),
            }
        });

        methods.add_method("tool_output_lines", |lua, _this, ()| {
            let cfg = config_table(lua)?;
            cfg.raw_get::<LuaValue>("tool_output_lines")
        });

        methods.add_method("record_read", |_lua, this, path: String| {
            this.tracker.record_read(&FileKey::new(Path::new(&path)));
            Ok(true)
        });

        methods.add_method("check_before_edit", |lua, this, path: String| {
            let stale_check = {
                let cfg = config_table(lua)?;
                cfg.raw_get::<bool>("stale_read_check")?
            };
            if !stale_check {
                return Ok((true, LuaValue::Nil));
            }
            let key = FileKey::new(Path::new(&path));
            match this.tracker.check_before_edit(&key) {
                Ok(()) => Ok((true, LuaValue::Nil)),
                Err(msg) => Ok((false, LuaValue::String(lua.create_string(msg.as_str())?))),
            }
        });

        methods.add_method("is_instruction_file", |_lua, _this, name: String| {
            Ok(is_instruction_file(&name))
        });

        methods.add_method("find_instructions", |lua, this, dir_path: String| {
            let cwd = std::env::current_dir().unwrap_or_default();
            let abs = if Path::new(&dir_path).is_absolute() {
                PathBuf::from(&dir_path)
            } else {
                cwd.join(&dir_path)
            };
            let results = find_subdirectory_instructions(&abs, &cwd, &this.instructions);
            let tbl = lua.create_table()?;
            for (i, (path, content)) in results.into_iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("path", path)?;
                entry.set("content", content)?;
                tbl.set(i + 1, entry)?;
            }
            Ok(tbl)
        });
    }
}

fn build_ctx(
    lua: &Lua,
    tracker: &Arc<FileAccess>,
    instructions: &LoadedInstructions,
) -> LuaResult<LuaValue> {
    let ud = lua.create_userdata(ChildCtx {
        tracker: tracker.clone(),
        instructions: instructions.clone(),
    })?;
    Ok(LuaValue::UserData(ud))
}

// ──────────────────────────────────────────────
//  maki.* API surface
// ──────────────────────────────────────────────

fn create_maki_api(lua: &Lua, forward: Option<HostForward>) -> Result<(), LuaError> {
    let maki = lua.create_table()?;

    // maki.fs
    let fs = lua.create_table()?;
    fs.set("read", lua.create_function(fs_read)?)?;
    fs.set("write", lua.create_function(fs_write)?)?;
    fs.set("metadata", lua.create_function(fs_metadata)?)?;
    fs.set("dirname", lua.create_function(fs_dirname)?)?;
    fs.set("basename", lua.create_function(fs_basename)?)?;
    fs.set("abspath", lua.create_function(fs_abspath)?)?;
    fs.set("dir", lua.create_function(fs_dir)?)?;
    fs.set("mkdir", lua.create_function(fs_mkdir)?)?;
    fs.set("rm", lua.create_function(fs_rm)?)?;
    fs.set("glob", lua.create_function(fs_glob)?)?;
    fs.set("grep", lua.create_function(fs_grep)?)?;
    maki.set("fs", fs)?;

    // maki.uv
    let uv = lua.create_table()?;
    uv.set("cwd", lua.create_function(uv_cwd)?)?;
    uv.set("os_homedir", lua.create_function(uv_os_homedir)?)?;
    uv.set("os_getenv", lua.create_function(uv_os_getenv)?)?;
    maki.set("uv", uv)?;

    // maki.log
    let log = lua.create_table()?;
    log.set("debug", lua.create_function(log_debug)?)?;
    log.set("info", lua.create_function(log_info)?)?;
    log.set("warn", lua.create_function(log_warn)?)?;
    log.set("error", lua.create_function(log_error)?)?;
    maki.set("log", log)?;

    // maki.ui: local stubs for pure helpers; everything else (anything with
    // a side effect on the host UI) derives a forwarder via the table
    // metatable when a forwarder is present.
    let ui = lua.create_table()?;
    ui.set("buf", lua.create_function(ui_buf)?)?;
    ui.set("highlight", lua.create_function(ui_highlight)?)?;
    ui.set("theme_color", lua.create_function(ui_theme_color)?)?;
    ui.set("humantime", lua.create_function(ui_humantime)?)?;
    if let Some(forward) = forward {
        let ui_mt = lua.create_table()?;
        ui_mt.set(
            "__index",
            lua.create_function(move |lua, (_, key): (LuaTable, LuaValue)| {
                let key = match &key {
                    LuaValue::String(s) => s.to_str()?.to_string(),
                    _ => {
                        return Err(LuaError::runtime("maki.ui index must be a string"));
                    }
                };
                let name = format!("{HOST_UI_PREFIX}{key}");
                let forward = Arc::clone(&forward);
                Ok(LuaValue::Function(lua.create_function(
                    move |lua, args: MultiValue| {
                        let json_args = args
                            .iter()
                            .map(|v| lua_to_json(lua, v))
                            .collect::<LuaResult<Vec<Value>>>()?;
                        if let Err(e) = forward(&name, json_args, Vec::new()) {
                            warn!(error = %e, "sandbox: forwarding {name} failed");
                        }
                        Ok(())
                    },
                )?))
            })?,
        )?;
        ui.set_metatable(Some(ui_mt))?;
    }
    maki.set("ui", ui)?;

    // maki.api
    let api = lua.create_table()?;
    api.set("register_tool", lua.create_function(api_register_tool)?)?;
    api.set(
        "register_options",
        lua.create_function(api_register_options)?,
    )?;
    api.set("register_prompt_hint", lua.create_function(api_noop)?)?;
    api.set("register_command", lua.create_function(api_noop)?)?;
    maki.set("api", api)?;

    // maki.fn (job management — synchronous in sandbox)
    let fn_tbl = lua.create_table()?;
    fn_tbl.set("jobstart", lua.create_function(fn_jobstart)?)?;
    fn_tbl.set("jobwait", lua.create_function(fn_jobwait)?)?;
    fn_tbl.set("jobstop", lua.create_function(fn_jobstop)?)?;
    maki.set("fn", fn_tbl)?;

    // maki.treesitter (stub)
    let ts = lua.create_table()?;
    ts.set("get_parser", lua.create_function(ts_get_parser)?)?;
    ts.set("get_node_text", lua.create_function(ts_get_node_text)?)?;
    maki.set("treesitter", ts)?;

    // maki.json
    let json_tbl = lua.create_table()?;
    json_tbl.set("encode", lua.create_function(json_encode)?)?;
    json_tbl.set("decode", lua.create_function(json_decode)?)?;
    maki.set("json", json_tbl)?;

    // maki.split
    maki.set("split", lua.create_function(maki_split)?)?;

    // maki.async (run inline — no async in child)
    let async_tbl = lua.create_table()?;
    async_tbl.set("run", lua.create_function(async_run)?)?;
    maki.set("async", async_tbl)?;

    lua.globals().set("maki", maki)?;
    lua.globals()
        .set("_registered_tools", lua.create_table()?)?;
    lua.globals().set("_loaded", lua.create_table()?)?;

    Ok(())
}

// ──────────────────────────────────────────────
//  require() resolution
// ──────────────────────────────────────────────

fn setup_require(lua: &Lua, plugin_dir: PathBuf) -> Result<(), LuaError> {
    let global_require = create_require_fn(lua, &plugin_dir, None)?;
    lua.globals().set("require", global_require)?;
    Ok(())
}

/// Build a `require` function. When `base` is set, plugin-local modules
/// (`{base}/foo.lua`) are resolved after the shared `lib/` modules, matching
/// the parent runtime's precedence.
fn create_require_fn(lua: &Lua, plugin_dir: &Path, base: Option<&str>) -> LuaResult<LuaFunction> {
    let plugin_dir = plugin_dir.to_path_buf();
    let base = base.map(str::to_string);
    lua.create_function(move |lua, name: String| {
        resolve_require(lua, &plugin_dir, base.as_deref(), &name)
    })
}

fn read_embedded_file(rel: &str) -> Option<String> {
    EMBEDDED_PLUGINS
        .get_file(rel)
        .and_then(|f| f.contents_utf8())
        .map(String::from)
}

fn resolve_require(
    lua: &Lua,
    plugin_dir: &Path,
    base: Option<&str>,
    modname: &str,
) -> LuaResult<LuaValue> {
    let loaded: LuaTable = lua.globals().get("_loaded")?;
    if let Ok(val) = loaded.get::<LuaValue>(modname)
        && !val.is_nil()
    {
        return Ok(val);
    }

    let rel = modname.replace('.', "/");
    let mut candidates = vec![format!("lib/{rel}.lua"), format!("lib/{rel}/init.lua")];
    if let Some(base) = base {
        candidates.push(format!("{base}/{rel}.lua"));
    }
    candidates.push(format!("{rel}.lua"));

    for path in &candidates {
        // Try filesystem first, then embedded
        let src = plugin_dir
            .join(path)
            .is_file()
            .then(|| std::fs::read_to_string(plugin_dir.join(path)).ok())
            .flatten()
            .or_else(|| read_embedded_file(path));

        if let Some(src) = src {
            let display_path = if plugin_dir.join(path).is_file() {
                plugin_dir.join(path).to_string_lossy().to_string()
            } else {
                path.clone()
            };

            let env = lua.create_table()?;
            // Inherit all standard globals (string, table, math, etc.)
            for pair in lua.globals().pairs::<LuaValue, LuaValue>() {
                let (k, v) = pair?;
                env.set(k, v)?;
            }
            env.set("require", lua.globals().get::<LuaFunction>("require")?)?;
            env.set("maki", lua.globals().get::<LuaValue>("maki")?)?;
            let result: LuaValue = lua
                .load(&src)
                .set_name(&display_path)
                .set_environment(env)
                .eval()
                .map_err(|e| LuaError::runtime(format!("require '{modname}': {e}")))?;
            loaded.set(modname, result.clone())?;
            return Ok(result);
        }
    }

    // Return nil instead of error — some optional modules may not exist
    Ok(LuaValue::Nil)
}

// ──────────────────────────────────────────────
//  Plugin loading
// ──────────────────────────────────────────────

fn load_plugins(lua: &Lua, plugin_dir: &Path) -> Result<(), LuaError> {
    // Try filesystem first, fall back to embedded
    let use_embedded = !plugin_dir.is_dir();
    if use_embedded {
        debug!("lua_runtime: plugin dir not found, using embedded plugins");
    }

    let subdirs: Vec<(String, PathBuf)> = if use_embedded {
        EMBEDDED_PLUGINS
            .dirs()
            .map(|d| {
                let name = d
                    .path()
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                (name, PathBuf::new())
            })
            .collect()
    } else {
        std::fs::read_dir(plugin_dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().is_ok_and(|ft| ft.is_dir()))
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        (name, e.path())
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    for (name, fs_path) in &subdirs {
        // Read init.lua from filesystem or embedded
        let src = if !use_embedded {
            let init_lua = fs_path.join("init.lua");
            if !init_lua.is_file() {
                continue;
            }
            match std::fs::read_to_string(&init_lua) {
                Ok(s) => s,
                Err(e) => {
                    warn!(plugin = %name, error = %e, "lua_runtime: cannot read");
                    continue;
                }
            }
        } else if let Some(file) = EMBEDDED_PLUGINS.get_file(format!("{name}/init.lua")) {
            match file.contents_utf8() {
                Some(s) => s.to_string(),
                None => continue,
            }
        } else {
            continue;
        };

        let env = lua.create_table()?;
        // Inherit all standard globals (string, table, math, etc.)
        for pair in lua.globals().pairs::<LuaValue, LuaValue>() {
            let (k, v) = pair?;
            env.set(k, v)?;
        }
        env.set("require", create_require_fn(lua, plugin_dir, Some(name))?)?;
        env.set("maki", lua.globals().get::<LuaValue>("maki")?)?;

        if let Err(e) = lua.load(&src).set_name(name).set_environment(env).exec() {
            warn!(plugin = %name, error = %e, "lua_runtime: load failed");
        } else {
            debug!(plugin = %name, "lua_runtime: loaded");
        }
    }

    let tools: LuaTable = lua.globals().get("_registered_tools")?;
    debug!(count = tools.raw_len(), "lua_runtime: plugins loaded");
    Ok(())
}

// ──────────────────────────────────────────────
//  Tool dispatch helpers
// ──────────────────────────────────────────────

/// Extract `(output, is_error)` from Lua handler return values.
///
/// Convention: `handler(input)` returns:
/// - `string` → success
/// - `nil, string` → error
/// - `table { llm_output, is_error? }` → structured result
fn extract_tool_result(values: LuaMultiValue) -> Result<(String, bool), String> {
    let mut iter = values.into_iter();
    match iter.next() {
        Some(LuaValue::String(s)) => {
            let text = s.to_str().map_err(|e| e.to_string())?.to_string();
            Ok((text, false))
        }
        Some(LuaValue::Table(t)) => {
            let output: String = t.get("llm_output").unwrap_or_default();
            let is_error: bool = t.get("is_error").unwrap_or(false);
            Ok((output, is_error))
        }
        Some(LuaValue::Nil) => match iter.next() {
            Some(LuaValue::String(err)) => {
                let msg = err.to_str().map_err(|e| e.to_string())?.to_string();
                Ok((msg, true))
            }
            _ => Ok(("tool returned nil".into(), true)),
        },
        _ => Ok(("tool returned unexpected type".into(), true)),
    }
}

// ──────────────────────────────────────────────
//  maki.fs.*  (returns (value, error))
// ──────────────────────────────────────────────

fn fs_read(lua: &Lua, path: String) -> LuaResult<LuaMultiValue> {
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(LuaMultiValue::from_vec(vec![LuaValue::String(
            lua.create_string(&content)?,
        )])),
        Err(e) => Ok(LuaMultiValue::from_vec(vec![
            LuaValue::Nil,
            LuaValue::String(lua.create_string(format!("read error: {e}").as_bytes())?),
        ])),
    }
}

fn fs_write(lua: &Lua, (path, content): (String, String)) -> LuaResult<LuaMultiValue> {
    if let Some(parent) = Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, &content) {
        Ok(()) => Ok(LuaMultiValue::from_vec(vec![
            LuaValue::Boolean(true),
            LuaValue::Nil,
        ])),
        Err(e) => Ok(LuaMultiValue::from_vec(vec![
            LuaValue::Nil,
            LuaValue::String(lua.create_string(format!("write error: {e}").as_bytes())?),
        ])),
    }
}

fn fs_metadata(lua: &Lua, path: String) -> LuaResult<LuaValue> {
    match std::fs::metadata(&path) {
        Ok(meta) => {
            let t = lua.create_table()?;
            t.set("is_dir", meta.is_dir())?;
            t.set("is_file", meta.is_file())?;
            t.set("size", meta.len())?;
            Ok(LuaValue::Table(t))
        }
        Err(_) => Ok(LuaValue::Nil),
    }
}

fn fs_dirname(lua: &Lua, path: String) -> LuaResult<LuaValue> {
    match Path::new(&path).parent() {
        Some(p) => Ok(LuaValue::String(
            lua.create_string(p.to_string_lossy().as_bytes())?,
        )),
        None => Ok(LuaValue::Nil),
    }
}

fn fs_basename(lua: &Lua, path: String) -> LuaResult<LuaValue> {
    match Path::new(&path).file_name() {
        Some(name) => Ok(LuaValue::String(
            lua.create_string(name.to_string_lossy().as_bytes())?,
        )),
        None => Ok(LuaValue::Nil),
    }
}

fn fs_abspath(lua: &Lua, path: String) -> LuaResult<LuaValue> {
    let expanded = expand_home(&path);
    let p = Path::new(&expanded);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| p.to_path_buf(), |cwd| cwd.join(p))
    };
    Ok(LuaValue::String(
        lua.create_string(abs.to_string_lossy().as_bytes())?,
    ))
}

fn expand_home(path: &str) -> String {
    match path {
        "~" => std::env::var("HOME").unwrap_or_default(),
        _ if path.starts_with("~/") => {
            format!(
                "{}{}",
                std::env::var("HOME").unwrap_or_default(),
                &path[1..]
            )
        }
        other => other.to_string(),
    }
}

fn fs_dir(lua: &Lua, path: String) -> LuaResult<LuaMultiValue> {
    let entries = match std::fs::read_dir(&path) {
        Ok(rd) => rd,
        Err(e) => {
            return Ok(LuaMultiValue::from_vec(vec![
                LuaValue::Nil,
                LuaValue::String(lua.create_string(format!("dir error: {e}").as_bytes())?),
            ]));
        }
    };

    let table = lua.create_table()?;
    for (idx, entry) in entries.flatten().enumerate() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().is_ok_and(|ft| ft.is_dir());
        let kind = if is_dir { "directory" } else { "file" };
        let inner = lua.create_table()?;
        inner.set(1, name)?;
        inner.set(2, kind)?;
        table.set(idx + 1, inner)?;
    }
    Ok(LuaMultiValue::from_vec(vec![
        LuaValue::Table(table),
        LuaValue::Nil,
    ]))
}

fn fs_mkdir(lua: &Lua, (path, opts): (String, Option<LuaTable>)) -> LuaResult<LuaMultiValue> {
    let parents = opts
        .and_then(|o| o.get::<bool>("parents").ok())
        .unwrap_or(false);
    let result = if parents {
        std::fs::create_dir_all(&path)
    } else {
        std::fs::create_dir(&path)
    };
    match result {
        Ok(()) => Ok(LuaMultiValue::from_vec(vec![
            LuaValue::Boolean(true),
            LuaValue::Nil,
        ])),
        Err(e) => Ok(LuaMultiValue::from_vec(vec![
            LuaValue::Nil,
            LuaValue::String(lua.create_string(format!("mkdir error: {e}").as_bytes())?),
        ])),
    }
}

fn fs_rm(lua: &Lua, path: String) -> LuaResult<LuaMultiValue> {
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(LuaMultiValue::from_vec(vec![LuaValue::Boolean(true)])),
        Err(e) => Ok(LuaMultiValue::from_vec(vec![
            LuaValue::Nil,
            LuaValue::String(lua.create_string(format!("rm error: {e}").as_bytes())?),
        ])),
    }
}

fn fs_glob(lua: &Lua, (pattern, opts): (String, Option<LuaTable>)) -> LuaResult<LuaMultiValue> {
    let search_path = opts
        .as_ref()
        .and_then(|o| o.get::<String>("path").ok())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let limit = opts
        .as_ref()
        .and_then(|o| o.get::<usize>("limit").ok())
        .unwrap_or(100);

    let root = Path::new(&search_path);
    let mut paths: Vec<String> = Vec::new();
    collect_glob_matches(root, &pattern, &mut paths, limit);

    let table = lua.create_table()?;
    for (i, p) in paths.iter().enumerate() {
        table.set(i + 1, p.as_str())?;
    }
    Ok(LuaMultiValue::from_vec(vec![
        LuaValue::Table(table),
        LuaValue::Nil,
    ]))
}

/// Simple recursive glob: split pattern on `**/`, match the last segment
/// as a suffix against files found by walking the directory tree.
fn collect_glob_matches(dir: &Path, pattern: &str, out: &mut Vec<String>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    let (prefix, suffix) = match pattern.split_once("**/") {
        Some((p, s)) => (Some(p), s),
        None => (None, pattern),
    };
    let walk_root = prefix.map_or_else(|| dir.to_path_buf(), |p| dir.join(p));

    let mut stack = vec![walk_root];
    while let Some(current) = stack.pop() {
        if out.len() >= limit {
            break;
        }
        let rd = match std::fs::read_dir(&current) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            if out.len() >= limit {
                break;
            }
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().unwrap_or_default();
                if name != ".git" && name != "node_modules" {
                    stack.push(path);
                }
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && glob_match(suffix, name)
            {
                out.push(path.to_string_lossy().to_string());
            }
        }
    }
}

/// Simple glob pattern match: `*` matches any chars except `/`, `?` matches one char.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_match_inner(&p, &t)
}

fn glob_match_inner(pattern: &[char], text: &[char]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut star_pi = usize::MAX;
    let mut star_ti = 0;

    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

fn fs_grep(lua: &Lua, (pattern, opts): (String, Option<LuaTable>)) -> LuaResult<LuaMultiValue> {
    let mut params = grep_tool::GrepParams::new(pattern);
    if let Some(ref opts) = opts {
        if let Ok(v) = opts.get::<String>("path") {
            params.path = Some(expand_home(&v));
        }
        if let Ok(v) = opts.get::<String>("include") {
            params.include = Some(v);
        }
        if let Ok(v) = opts.get::<usize>("context_before") {
            params.context_before = v;
        }
        if let Ok(v) = opts.get::<usize>("context_after") {
            params.context_after = v;
        }
        if let Ok(v) = opts.get::<usize>("limit") {
            params.limit = v;
        }
        if let Ok(v) = opts.get::<usize>("max_line_bytes") {
            params.max_line_bytes = v;
        }
    }

    let (base, entries) = match grep_tool::grep_search(params) {
        Ok(r) => r,
        Err(e) => {
            return Ok(LuaMultiValue::from_vec(vec![
                LuaValue::Nil,
                LuaValue::String(lua.create_string(e.as_bytes())?),
            ]));
        }
    };

    let table = lua.create_table()?;
    for (i, entry) in entries.iter().enumerate() {
        let etbl = lua.create_table()?;
        etbl.set("path", base.join(&entry.path).to_string_lossy().as_ref())?;
        let groups_tbl = lua.create_table()?;
        for (gi, group) in entry.groups.iter().enumerate() {
            let gtbl = lua.create_table()?;
            let lines_tbl = lua.create_table()?;
            for (li, line) in group.lines.iter().enumerate() {
                let ltbl = lua.create_table()?;
                ltbl.set("line_nr", line.line_nr)?;
                ltbl.set("text", line.text.as_str())?;
                ltbl.set("is_match", line.is_match)?;
                lines_tbl.set(li + 1, ltbl)?;
            }
            gtbl.set("lines", lines_tbl)?;
            groups_tbl.set(gi + 1, gtbl)?;
        }
        etbl.set("groups", groups_tbl)?;
        table.set(i + 1, etbl)?;
    }
    Ok(LuaMultiValue::from_vec(vec![
        LuaValue::Table(table),
        LuaValue::Nil,
    ]))
}

// ──────────────────────────────────────────────
//  maki.uv.*
// ──────────────────────────────────────────────

fn uv_cwd(lua: &Lua, _: ()) -> LuaResult<LuaValue> {
    match std::env::current_dir() {
        Ok(p) => Ok(LuaValue::String(
            lua.create_string(p.to_string_lossy().as_bytes())?,
        )),
        Err(_) => Ok(LuaValue::Nil),
    }
}

fn uv_os_homedir(lua: &Lua, _: ()) -> LuaResult<LuaValue> {
    match std::env::var("HOME") {
        Ok(h) => Ok(LuaValue::String(lua.create_string(h.as_bytes())?)),
        Err(_) => Ok(LuaValue::Nil),
    }
}

fn uv_os_getenv(lua: &Lua, key: String) -> LuaResult<LuaValue> {
    match std::env::var(&key) {
        Ok(val) => Ok(LuaValue::String(lua.create_string(val.as_bytes())?)),
        Err(_) => Ok(LuaValue::Nil),
    }
}

// ──────────────────────────────────────────────
//  maki.log.*
// ──────────────────────────────────────────────

fn log_debug(_: &Lua, msg: String) -> LuaResult<()> {
    tracing::debug!(msg, "lua plugin");
    Ok(())
}
fn log_info(_: &Lua, msg: String) -> LuaResult<()> {
    tracing::info!(msg, "lua plugin");
    Ok(())
}
fn log_warn(_: &Lua, msg: String) -> LuaResult<()> {
    tracing::warn!(msg, "lua plugin");
    Ok(())
}
fn log_error(_: &Lua, msg: String) -> LuaResult<()> {
    tracing::error!(msg, "lua plugin");
    Ok(())
}

// ──────────────────────────────────────────────
//  maki.ui.*  (stubs)
// ──────────────────────────────────────────────

fn ui_buf(lua: &Lua, _: ()) -> LuaResult<LuaValue> {
    let buf = lua.create_table()?;
    let noop = lua.create_function(|_, _: LuaValue| Ok(LuaValue::Nil))?;
    buf.set("line", noop)?;
    let noop_multi = lua.create_function(|_, _: Vec<LuaValue>| Ok(LuaValue::Nil))?;
    buf.set("on", noop_multi)?;
    buf.set(
        "set_lines",
        lua.create_function(|_, _: Vec<LuaValue>| Ok(LuaValue::Nil))?,
    )?;
    Ok(LuaValue::Table(buf))
}

fn ui_highlight(
    _: &Lua,
    (_source, _ext, _opts): (String, String, Option<LuaTable>),
) -> LuaResult<LuaValue> {
    Ok(LuaValue::Nil)
}

fn ui_theme_color(_: &Lua, _: String) -> LuaResult<LuaValue> {
    Ok(LuaValue::Nil)
}

fn ui_humantime(_: &Lua, secs: u64) -> LuaResult<String> {
    Ok(format!("{secs}s"))
}

// ──────────────────────────────────────────────
//  maki.api.*
// ──────────────────────────────────────────────

fn api_register_tool(lua: &Lua, spec: LuaTable) -> LuaResult<()> {
    let name: String = spec
        .get("name")
        .map_err(|_| LuaError::runtime("register_tool: missing 'name'"))?;
    let handler: LuaFunction = spec
        .get("handler")
        .map_err(|_| LuaError::runtime("register_tool: missing 'handler'"))?;

    let tools: LuaTable = lua.globals().get("_registered_tools")?;
    tools.set(name.as_str(), handler)?;
    Ok(())
}

fn api_register_options(lua: &Lua, spec: LuaTable) -> LuaResult<LuaValue> {
    // Return the resolved options table (defaults only — the child has no
    // access to the user's `plugins.<name>` overrides), matching the
    // parent contract where a key is absent when there is no default.
    let merged = lua.create_table()?;
    for pair in spec.pairs::<String, LuaValue>() {
        let (name, val) = pair?;
        if let LuaValue::Table(entry) = &val
            && let Ok(default) = entry.raw_get::<LuaValue>("default")
            && !default.is_nil()
        {
            merged.set(name.as_str(), default)?;
        }
    }
    Ok(LuaValue::Table(merged))
}

fn api_noop(_: &Lua, _: LuaValue) -> LuaResult<()> {
    Ok(())
}

// ──────────────────────────────────────────────
//  maki.fn.*  (synchronous process execution)
// ──────────────────────────────────────────────

fn fn_jobstart(lua: &Lua, (command, opts): (String, Option<LuaTable>)) -> LuaResult<LuaValue> {
    let workdir = opts.as_ref().and_then(|o| o.get::<String>("cwd").ok());

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&command);
    if let Some(dir) = &workdir {
        cmd.current_dir(dir);
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = cmd
        .output()
        .map_err(|e| LuaError::runtime(format!("jobstart: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    let result = lua.create_table()?;
    result.set("stdout", stdout.as_ref())?;
    result.set("stderr", stderr.as_ref())?;
    result.set("exit_code", exit_code)?;
    Ok(LuaValue::Table(result))
}

fn fn_jobwait(_: &Lua, (id, _timeout): (LuaValue, Option<u64>)) -> LuaResult<LuaValue> {
    // In synchronous mode, jobstart already returned the result.
    // If id is a table (the result from jobstart), return it directly.
    Ok(id)
}

fn fn_jobstop(_: &Lua, _: LuaValue) -> LuaResult<()> {
    Ok(())
}

// ──────────────────────────────────────────────
//  maki.treesitter.*  (stubs)
// ──────────────────────────────────────────────

fn ts_get_parser(_: &Lua, (_source, _lang): (String, String)) -> LuaResult<LuaValue> {
    Ok(LuaValue::Nil)
}

fn ts_get_node_text(lua: &Lua, (_node, _src): (LuaValue, String)) -> LuaResult<LuaValue> {
    Ok(LuaValue::String(lua.create_string(b"")?))
}

// ──────────────────────────────────────────────
//  maki.json.*
// ──────────────────────────────────────────────

fn json_encode(lua: &Lua, value: LuaValue) -> LuaResult<String> {
    let json_val = lua_to_json(lua, &value)?;
    serde_json::to_string(&json_val).map_err(|e| LuaError::runtime(e.to_string()))
}

fn json_decode(lua: &Lua, text: String) -> LuaResult<LuaValue> {
    let json_val: Value = serde_json::from_str(&text)
        .map_err(|e| LuaError::runtime(format!("json decode: {e}")))?;
    json_to_lua(lua, &json_val)
}

// ──────────────────────────────────────────────
//  maki.split
// ──────────────────────────────────────────────

fn maki_split(lua: &Lua, (text, sep): (String, String)) -> LuaResult<LuaValue> {
    let table = lua.create_table()?;
    for (i, part) in text.split(&sep).enumerate() {
        table.set(i + 1, part)?;
    }
    Ok(LuaValue::Table(table))
}

// ──────────────────────────────────────────────
//  maki.async.run  (run inline)
// ──────────────────────────────────────────────

fn async_run(_lua: &Lua, f: LuaFunction) -> LuaResult<LuaValue> {
    // Just call the function inline — no async in sandbox child
    let _: LuaValue = f.call(())?;
    Ok(LuaValue::Nil)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_plugin_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn empty_plugin_dir_loads_cleanly() {
        let dir = tmp_plugin_dir();
        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        // No tools registered
        let err = rt.call_tool("read", &[], &[]).unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn register_tool_and_call() {
        let dir = tmp_plugin_dir();
        std::fs::create_dir(dir.path().join("echo")).unwrap();
        std::fs::write(
            dir.path().join("echo/init.lua"),
            r#"
            maki.api.register_tool({
                name = "echo",
                description = "echo tool",
                schema = { type = "object", properties = {} },
                handler = function(input)
                    return input.text or "nothing"
                end,
            })
            "#,
        )
        .unwrap();

        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        let (output, is_error) = rt
            .call_tool("echo", &[], &[("text".into(), json!("hello"))])
            .unwrap();
        assert_eq!(output, "hello");
        assert!(!is_error);
    }

    #[test]
    fn handler_returning_table() {
        let dir = tmp_plugin_dir();
        std::fs::create_dir(dir.path().join("t")).unwrap();
        std::fs::write(
            dir.path().join("t/init.lua"),
            r#"
            maki.api.register_tool({
                name = "t",
                description = "test",
                schema = { type = "object", properties = {} },
                handler = function(input)
                    return { llm_output = "ok", is_error = false }
                end,
            })
            "#,
        )
        .unwrap();

        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        let (output, is_error) = rt.call_tool("t", &[], &[]).unwrap();
        assert_eq!(output, "ok");
        assert!(!is_error);
    }

    #[test]
    fn handler_returning_error() {
        let dir = tmp_plugin_dir();
        std::fs::create_dir(dir.path().join("e")).unwrap();
        std::fs::write(
            dir.path().join("e/init.lua"),
            r#"
            maki.api.register_tool({
                name = "e",
                description = "error tool",
                schema = { type = "object", properties = {} },
                handler = function(input)
                    return nil, "something went wrong"
                end,
            })
            "#,
        )
        .unwrap();

        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        let (output, is_error) = rt.call_tool("e", &[], &[]).unwrap();
        assert_eq!(output, "something went wrong");
        assert!(is_error);
    }

    #[test]
    fn fs_read_works_in_sandbox() {
        let dir = tmp_plugin_dir();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello sandbox").unwrap();

        std::fs::create_dir(dir.path().join("reader")).unwrap();
        std::fs::write(
            dir.path().join("reader/init.lua"),
            r#"
            maki.api.register_tool({
                name = "reader",
                description = "read test",
                schema = { type = "object", properties = {} },
                handler = function(input)
                    local content, err = maki.fs.read(input.path)
                    if not content then
                        return { llm_output = err, is_error = true }
                    end
                    return content
                end,
            })
            "#,
        )
        .unwrap();

        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        let (output, _) = rt
            .call_tool(
                "reader",
                &[],
                &[("path".into(), json!(file.to_str().unwrap()))],
            )
            .unwrap();
        assert_eq!(output, "hello sandbox");
    }

    #[test]
    fn maki_split_works() {
        let dir = tmp_plugin_dir();
        std::fs::create_dir(dir.path().join("spliter")).unwrap();
        std::fs::write(
            dir.path().join("spliter/init.lua"),
            r#"
            maki.api.register_tool({
                name = "spliter",
                description = "split test",
                schema = { type = "object", properties = {} },
                handler = function(input)
                    local parts = maki.split("a,b,c", ",")
                    return table.concat(parts, "|")
                end,
            })
            "#,
        )
        .unwrap();

        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        let (output, _) = rt.call_tool("spliter", &[], &[]).unwrap();
        assert_eq!(output, "a|b|c");
    }

    #[test]
    fn ui_calls_forward_to_the_host() {
        type ForwardedCall = (String, Vec<Value>, Vec<(String, Value)>);
        let seen: Arc<std::sync::Mutex<Vec<ForwardedCall>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let forward_seen = Arc::clone(&seen);
        let forward: HostForward = Arc::new(move |name, args, kwargs| {
            forward_seen
                .lock()
                .unwrap()
                .push((name.to_string(), args, kwargs));
            Ok(String::new())
        });
        let rt = ChildLuaRuntime::with_forwarder(tmp_plugin_dir().path(), forward).unwrap();

        let maki: LuaTable = rt.lua.globals().get("maki").unwrap();
        let ui: LuaTable = maki.get("ui").unwrap();
        let set_status_hint: LuaFunction = ui.get("set_status_hint").unwrap();

        let spans = rt.lua.create_table().unwrap();
        let row = rt.lua.create_table().unwrap();
        row.set(1, "q").unwrap();
        row.set(2, "quit").unwrap();
        spans.set(1, row).unwrap();
        set_status_hint
            .call::<()>(spans)
            .expect("hint table forwarded");

        set_status_hint
            .call::<()>(LuaValue::Nil)
            .expect("nil hint forwarded");

        let flash: LuaFunction = ui.get("flash").unwrap();
        flash.call::<()>("flash!").expect("ui call forwarded");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].0, "maki.ui.set_status_hint");
        assert_eq!(seen[0].1, vec![json!([["q", "quit"]])]);
        assert!(seen[0].2.is_empty());
        assert_eq!(seen[1].0, "maki.ui.set_status_hint");
        assert_eq!(seen[1].1, vec![Value::Null]);
        assert!(seen[1].2.is_empty());
        assert_eq!(seen[2].0, "maki.ui.flash");
        assert_eq!(seen[2].1, vec![json!("flash!")]);
        assert!(seen[2].2.is_empty());
    }

    #[test]
    fn ui_stubs_do_not_forward() {
        let seen: Arc<std::sync::Mutex<u32>> = Arc::new(std::sync::Mutex::new(0));
        let forward_seen = Arc::clone(&seen);
        let forward: HostForward = Arc::new(move |_, _, _| {
            *forward_seen.lock().unwrap() += 1;
            Ok(String::new())
        });
        let rt = ChildLuaRuntime::with_forwarder(tmp_plugin_dir().path(), forward).unwrap();

        let maki: LuaTable = rt.lua.globals().get("maki").unwrap();
        let ui: LuaTable = maki.get("ui").unwrap();
        let buf: LuaFunction = ui.get("buf").unwrap();
        buf.call::<LuaValue>(()).unwrap();
        assert_eq!(*seen.lock().unwrap(), 0);
    }

    #[test]
    fn ui_forwarded_calls_are_absent_without_a_forwarder() {
        let rt = ChildLuaRuntime::new(tmp_plugin_dir().path()).unwrap();
        let maki: LuaTable = rt.lua.globals().get("maki").unwrap();
        let ui: LuaTable = maki.get("ui").unwrap();
        assert!(ui.get::<LuaValue>("set_status_hint").unwrap().is_nil());
        assert!(ui.get::<LuaValue>("flash").unwrap().is_nil());
    }
}

#[cfg(test)]
mod embedded_plugins {
    use super::*;
    use serde_json::json;

    const MISSING_PLUGIN_DIR: &str = "/nonexistent/plugins/path";
    const AGENT_CONFIG_JSON: &str = r#"{"max_output_lines":2000,"max_output_bytes":51200,"stale_read_check":true,"tool_output_lines":{"bash":5,"code_execution":5,"task":5,"index":3,"grep":3,"read":3,"write":7,"web":3,"other":3}}"#;

    fn embedded_runtime() -> ChildLuaRuntime {
        let rt = ChildLuaRuntime::new(Path::new(MISSING_PLUGIN_DIR)).unwrap();
        rt.set_config(AGENT_CONFIG_JSON).unwrap();
        rt
    }

    #[test]
    fn embedded_plugins_register_child_tools() {
        let rt = embedded_runtime();
        let names = rt.registered_tool_names().unwrap();
        for want in ["read", "write", "edit", "glob", "list", "grep"] {
            assert!(names.iter().any(|n| n == want), "{want} missing: {names:?}");
        }
    }

    #[test]
    fn embedded_read_tool_returns_numbered_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("doc.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").unwrap();

        let rt = embedded_runtime();
        let (out, err) = rt
            .call_tool(
                "read",
                &[],
                &[
                    ("path".into(), json!(file.to_str().unwrap())),
                    ("offset".into(), json!(1)),
                    ("limit".into(), json!(2)),
                ],
            )
            .unwrap();
        assert!(!err, "{out}");
        assert_eq!(
            out,
            "1: alpha\n2: beta\n\n...\n\nTruncated lines: 3-4. Use offset=3 to read further."
        );
    }

    #[test]
    fn embedded_grep_tool_finds_matches() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo target\nthree\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "no hits\n").unwrap();

        let rt = embedded_runtime();
        let (out, err) = rt
            .call_tool(
                "grep",
                &[],
                &[
                    ("pattern".into(), json!("target")),
                    ("path".into(), json!(dir.path().to_str().unwrap())),
                ],
            )
            .unwrap();
        assert!(!err, "{out}");
        assert!(out.contains("a.txt"), "{out}");
        assert!(out.contains("2: two target"), "{out}");
    }

    #[test]
    fn ctx_exposes_config_tracker_and_instructions() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("probe")).unwrap();
        std::fs::write(
            dir.path().join("probe/init.lua"),
            r#"
            maki.api.register_tool({
                name = "probe",
                description = "probe the child ctx",
                schema = { type = "object", properties = {} },
                handler = function(input, ctx)
                    local max_lines = ctx:config("max_output_lines", 111)
                    local stale = ctx:config("stale_read_check")
                    local tol = ctx:tool_output_lines()
                    local read_tol = tol and tol.read or nil
                    ctx:record_read(input.path)
                    local ok, err = ctx:check_before_edit(input.path)
                    local is_instr = ctx:is_instruction_file("AGENTS.md")
                    local instrs = ctx:find_instructions(input.path)
                    assert(type(instrs) == "table")
                    return string.format("%s|%s|%s|%s|%s|%s",
                        max_lines, tostring(stale), tostring(read_tol),
                        tostring(ok), tostring(err), tostring(is_instr))
                end,
            })
            "#,
        )
        .unwrap();
        let file = dir.path().join("target.txt");
        std::fs::write(&file, "hello").unwrap();

        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        rt.set_config(AGENT_CONFIG_JSON).unwrap();
        let (out, err) = rt
            .call_tool(
                "probe",
                &[],
                &[("path".into(), json!(file.to_str().unwrap()))],
            )
            .unwrap();
        assert!(!err, "{out}");
        assert_eq!(out, "2000|true|3|true|nil|true");
    }

    #[test]
    fn register_options_returns_merged_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("opts_probe")).unwrap();
        std::fs::write(
            dir.path().join("opts_probe/init.lua"),
            r#"
            local opts = maki.api.register_options({
                with_default = { default = 42, desc = "a number" },
                no_default = { type = "integer", desc = "no default" },
            })
            maki.api.register_tool({
                name = "opts_probe",
                description = "report opts",
                schema = { type = "object", properties = {} },
                handler = function(_input, _ctx)
                    return tostring(opts.with_default) .. "|" .. tostring(opts.no_default)
                end,
            })
            "#,
        )
        .unwrap();

        let rt = ChildLuaRuntime::new(dir.path()).unwrap();
        let (out, err) = rt.call_tool("opts_probe", &[], &[]).unwrap();
        assert!(!err, "{out}");
        assert_eq!(out, "42|nil");
    }

    #[test]
    fn run_init_file_registers_custom_tool() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("init.lua"),
            r#"
            maki.api.register_tool({
                name = "custom_probe",
                description = "probe user init.lua",
                schema = { type = "object", properties = {} },
                handler = function(_input, _ctx)
                    return "ran-custom"
                end,
            })
            "#,
        )
        .unwrap();

        let rt = ChildLuaRuntime::new(Path::new(MISSING_PLUGIN_DIR)).unwrap();
        rt.run_init_file(&dir.path().join("init.lua")).unwrap();
        assert!(
            rt.registered_tool_names()
                .unwrap()
                .iter()
                .any(|n| n == "custom_probe")
        );
        let (out, err) = rt.call_tool("custom_probe", &[], &[]).unwrap();
        assert!(!err, "{out}");
        assert_eq!(out, "ran-custom");
    }

    #[test]
    fn run_init_file_missing_file_is_ok() {
        let rt = ChildLuaRuntime::new(Path::new(MISSING_PLUGIN_DIR)).unwrap();
        let missing = tempfile::TempDir::new().unwrap().path().join("init.lua");
        assert!(rt.run_init_file(&missing).is_err());
    }
}
