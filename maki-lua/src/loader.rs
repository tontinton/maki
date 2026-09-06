use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use include_dir::{Dir, File, include_dir};
use maki_agent::SessionEndReason;
use maki_agent::permissions::{PluginRuleStore, carries_builtin_defaults};
use maki_agent::tools::{ToolRegistry, ToolSource};
use maki_config::{PluginsConfig, RawConfig};

use crate::api::keymap::KeymapReader;
use crate::api::options::{PluginOptionSpecs, PluginOpts};
use crate::api::util::command::{HintReader, LuaCommandReader, UiAction, UiAttachment};
use crate::error::PluginError;
use crate::pack::DiscoveredPackage;
use crate::plugin_permissions::{
    MANIFEST_FILE, PluginPermissions, Requested, check_plugin_compatibility,
    load_plugin_permissions,
};
use crate::runtime::{
    self, ClickFallback, ConfigScope, EndSession, LoadChunk, LoadContext, LuaThread, Request,
    RestoreItem,
};
use maki_agent::prompt::ResolvedSlots;
use maki_storage::id::MakiId;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const PACK_STATE_UNAVAILABLE: &str = "could not read package state: plugin host stopped";
const USER_PLUGIN: &str = "user";
pub const SKIPPED_PLUGIN_WARNING: &str = "skipping plugin lua";
/// Tests assert on this exact text, so a wording tweak here updates them too.
pub const PERMISSION_NAME_WARNING: &str = "inherits maki's permission rules for the builtin \
     tool of the same name, together with any \"always allow\" you saved";

struct BundledPlugin {
    name: &'static str,
    dir: Dir<'static>,
}

/// `lib` is not a default builtin; it exists so plugins can
/// `require()` shared modules across boundaries.
static BUNDLED_PLUGINS: &[BundledPlugin] = &[
    BundledPlugin {
        name: "sessions",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/sessions"),
    },
    BundledPlugin {
        name: "index",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/index"),
    },
    BundledPlugin {
        name: "webfetch",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/webfetch"),
    },
    BundledPlugin {
        name: "websearch",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/websearch"),
    },
    BundledPlugin {
        name: "bash",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/bash"),
    },
    BundledPlugin {
        name: "batch",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/batch"),
    },
    BundledPlugin {
        name: "grep",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/grep"),
    },
    BundledPlugin {
        name: "glob",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/glob"),
    },
    BundledPlugin {
        name: "skill",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/skill"),
    },
    BundledPlugin {
        name: "question",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/question"),
    },
    BundledPlugin {
        name: "todo_write",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/todo_write"),
    },
    BundledPlugin {
        name: "read",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/read"),
    },
    BundledPlugin {
        name: "write",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/write"),
    },
    BundledPlugin {
        name: "edit",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/edit"),
    },
    // Below the tools it pre-approves: `memory` allows writes into its own
    // state dir, and a rule can only name a registered tool.
    BundledPlugin {
        name: "memory",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/memory"),
    },
    BundledPlugin {
        name: "task",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/task"),
    },
    BundledPlugin {
        name: "code_execution",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/code_execution"),
    },
    BundledPlugin {
        name: "view_image",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/view_image"),
    },
    BundledPlugin {
        name: "lib",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/lib"),
    },
    BundledPlugin {
        name: "list",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/list"),
    },
    BundledPlugin {
        name: "file_mention",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/file_mention"),
    },
];

/// Every bundled name, not just the default-enabled ones. An external package
/// sharing an owner name with any of them would let one package's unload tear
/// down the other's registrations.
pub(crate) fn is_bundled(name: &str) -> bool {
    BUNDLED_PLUGINS.iter().any(|p| p.name == name)
}

/// A bundled plugin declares its permissions the same way an external one
/// does. Shipping inside the binary buys no implicit grant, so an unreadable
/// manifest fails the load rather than quietly widening anything, and a newly
/// guarded function stays denied until the plugin asks for it.
fn bundled_permissions(plugin: &BundledPlugin) -> Result<PluginPermissions, PluginError> {
    let fail = |message: String| PluginError::BundledManifest {
        plugin: plugin.name.to_owned(),
        message,
    };
    let source = plugin
        .dir
        .get_file(MANIFEST_FILE)
        .and_then(File::contents_utf8)
        .ok_or_else(|| fail(format!("no {MANIFEST_FILE} next to init.lua")))?;
    toml::from_str::<toml::Value>(source)
        .map(|manifest| Requested::from_manifest(&manifest).granted())
        .map_err(|e| fail(e.to_string()))
}

pub(crate) fn lib_dir() -> &'static Dir<'static> {
    &BUNDLED_PLUGINS
        .iter()
        .find(|p| p.name == "lib")
        .expect("lib plugin bundled")
        .dir
}

static BUNDLED_DIRS: LazyLock<&'static [&'static Dir<'static>]> = LazyLock::new(|| {
    let dirs: Vec<&'static Dir<'static>> = BUNDLED_PLUGINS.iter().map(|p| &p.dir).collect();
    Vec::leak(dirs)
});

/// A package's entrypoints: every `plugin/*.lua`, sorted by filename so load
/// order is deterministic across machines.
///
/// A repository can commit a symlink, so each entry is resolved and checked to
/// be inside the package before it is read.
fn package_entrypoints(root: &Path) -> Result<Vec<PathBuf>, PluginError> {
    let entrypoint_dir = root.join("plugin");
    let entries = match fs::read_dir(&entrypoint_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(PluginError::Io {
                path: entrypoint_dir,
                source,
            });
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| PluginError::Io {
            path: entrypoint_dir.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lua") {
            continue;
        }
        let resolved = path.canonicalize().map_err(|e| PluginError::Io {
            path: path.clone(),
            source: e,
        })?;
        if !resolved.starts_with(root) {
            return Err(PluginError::PackageEscape { path });
        }
        if resolved.is_file() {
            let Some(file_name) = path.file_name().map(std::ffi::OsString::from) else {
                continue;
            };
            files.push((file_name, resolved));
        }
    }
    // By file name, not by the resolved path: a package may symlink an entry
    // elsewhere inside itself, and load order must still be the order a user
    // sees in `plugin/`.
    files.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(files.into_iter().map(|(_, path)| path).collect())
}

pub struct PluginHost {
    inner: LuaThread,
    plugin_rules: Arc<PluginRuleStore>,
    registry: Arc<ToolRegistry>,
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        let Some(handle) = self.inner.join.take() else {
            return;
        };
        // Start the shutdown first, or the join below waits for all
        // queued bulk work to drain.
        self.begin_shutdown();
        let (done_tx, done_rx) = flume::bounded(1);
        std::thread::spawn(move || {
            let _ = done_tx.send(handle.join().is_err());
        });
        match done_rx.recv_timeout(SHUTDOWN_TIMEOUT) {
            Ok(true) => tracing::warn!("lua thread panicked on shutdown"),
            Err(_) => tracing::warn!("lua thread did not stop within timeout, detaching"),
            Ok(false) => {}
        }
    }
}

impl PluginHost {
    pub fn new(registry: Arc<ToolRegistry>) -> Result<Self, PluginError> {
        Self::with_jit(registry, true)
    }

    /// `jit: false` (the `--no-jit` flag) runs plugin Lua on the O1
    /// interpreter with full debug info. Applied at VM creation, so
    /// every chunk gets it, init.lua files included.
    pub fn with_jit(registry: Arc<ToolRegistry>, jit: bool) -> Result<Self, PluginError> {
        let plugin_rules = Arc::new(PluginRuleStore::default());
        let lua = runtime::spawn(
            Arc::clone(&registry),
            *BUNDLED_DIRS,
            jit,
            Arc::clone(&plugin_rules),
        )?;
        Ok(Self {
            inner: lua,
            plugin_rules,
            registry,
        })
    }

    /// The store that `maki.api.register_permission_rule` writes into. Hand
    /// it to every [`maki_agent::permissions::PermissionManager`] so plugin
    /// rules apply to all sessions.
    pub fn plugin_rules(&self) -> Arc<PluginRuleStore> {
        Arc::clone(&self.plugin_rules)
    }

    /// Stop the Lua thread from taking new work without joining it, so the
    /// caller can rebuild shared state (like the tool registry) while the
    /// old VM winds down on its own. The flag makes the watchdog abort
    /// in-flight callbacks, `Shutdown` on the priority lane skips ahead of
    /// queued bulk work, and swapping the senders for disconnected ones
    /// makes every later host call fail right at the send; `&mut self`
    /// rules out a call racing the swap. `Drop` still joins the thread.
    pub fn begin_shutdown(&mut self) {
        self.inner.shutdown.store(true, Ordering::Release);
        let _ = self.inner.prio_tx.send(Request::Shutdown);
        self.inner.tx = flume::unbounded().0;
        self.inner.prio_tx = flume::unbounded().0;
    }

    /// Boots the runtime and loads every default bundled plugin into `registry`.
    /// For callers like tests and docgen that want the full builtin set
    /// without building a config.
    pub fn with_all_builtins(registry: Arc<ToolRegistry>) -> Result<Self, PluginError> {
        let mut host = Self::new(registry)?;
        host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))?;
        Ok(host)
    }

    /// `warnings` collects non-fatal startup problems (an incompatible
    /// `plugin.toml` skips that directory's Lua) for the caller to surface.
    pub fn load_init_files(
        &self,
        cwd: &Path,
        warnings: &mut Vec<String>,
    ) -> Result<Option<RawConfig>, PluginError> {
        let mut merged: Option<RawConfig> = None;

        for global_dir in maki_storage::paths::config_search_dirs() {
            self.run_init_file(
                &global_dir.join("init.lua"),
                ConfigScope::Global,
                &mut merged,
                warnings,
            )?;
            if merged.is_some() {
                break;
            }
        }
        self.run_init_file(
            &cwd.join(".maki/init.lua"),
            ConfigScope::Project,
            &mut merged,
            warnings,
        )?;

        Ok(merged)
    }

    /// `--no-plugins` recovery path: skip every user `init.lua` while the
    /// host and builtin plugins stay live. Centralized so every entry point
    /// (TUI, index, acp, prompt) honors the flag identically.
    pub fn load_init_files_or_skip(
        &self,
        no_plugins: bool,
        cwd: &Path,
        warnings: &mut Vec<String>,
    ) -> Result<Option<RawConfig>, PluginError> {
        if no_plugins {
            return Ok(None);
        }
        self.load_init_files(cwd, warnings)
    }

    fn run_init_file(
        &self,
        path: &Path,
        scope: ConfigScope,
        merged: &mut Option<RawConfig>,
        warnings: &mut Vec<String>,
    ) -> Result<(), PluginError> {
        if !path.is_file() {
            return Ok(());
        }
        let source = fs::read_to_string(path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let plugin_dir = path.parent().map(Path::to_path_buf);
        if let Err(e) = check_plugin_compatibility(scope.label(), plugin_dir.as_deref()) {
            warnings.push(format!("{SKIPPED_PLUGIN_WARNING}: {e}"));
            return Ok(());
        }
        let owner = scope.label().to_owned();
        if let Some(raw) = self.send_config_lua(source, scope, plugin_dir)? {
            match merged {
                Some(existing) => existing.merge(raw),
                None => *merged = Some(raw),
            }
        }
        warnings.extend(self.permission_name_warning(&owner));
        Ok(())
    }

    pub fn load_builtins(&mut self, config: &PluginsConfig) -> Result<(), PluginError> {
        let result = self.send_builtin_loads(config);
        // Armed even when a load failed, so a caller that only warns about the
        // error is not left interpreting for the rest of the session.
        let _ = self.inner.tx.send(Request::WarmJit);
        result
    }

    fn send_builtin_loads(&self, config: &PluginsConfig) -> Result<(), PluginError> {
        for (plugin, opts) in &config.opts {
            // An enabled package takes its options when the package itself
            // loads, and an enabled builtin takes them in the loop below.
            if config.packages.contains(plugin) || config.names.contains(plugin) {
                continue;
            }
            // What is left is a name that exists but is not loading: a builtin
            // or a package the config disabled, or one discovery refused. It
            // cannot be a typo, because the config layer validated every
            // `plugins.<name>` key against the same names before this ran. A
            // package used to reach this as an error, which stopped maki from
            // starting over options it was already ignoring.
            let keys: Vec<&str> = opts.keys().map(String::as_str).collect();
            tracing::warn!(
                plugin = plugin.as_str(),
                keys = keys.join(", "),
                "nothing named {} is loading; its plugins.{} options are ignored",
                plugin,
                plugin
            );
        }
        if let Some(unknown) = config
            .names
            .iter()
            .find(|name| !BUNDLED_PLUGINS.iter().any(|p| p.name == name.as_str()))
        {
            return Err(PluginError::UnknownPlugin {
                plugin: unknown.clone(),
            });
        }
        // `BUNDLED_PLUGINS` order, not `config.names` order, because a rule can
        // only name a registered tool, so whoever owns a tool loads before
        // whoever pre-approves it. `DEFAULT_BUILTINS` stays alphabetical for
        // the config surface.
        for bundled in BUNDLED_PLUGINS {
            let Some(builtin) = config.names.iter().find(|n| n.as_str() == bundled.name) else {
                continue;
            };
            let dir = &bundled.dir;
            let init = dir
                .get_file("init.lua")
                .and_then(|f| f.contents_utf8())
                .ok_or_else(|| PluginError::Lua {
                    plugin: builtin.clone(),
                    source: mlua::Error::runtime("bundled plugin missing init.lua"),
                })?;
            let permissions = bundled_permissions(bundled)?;
            let name: Arc<str> = Arc::from(builtin.as_str());
            let opts = config
                .opts
                .get(builtin.as_str())
                .cloned()
                .map(Arc::new)
                .unwrap_or_default();
            self.send_load(
                Arc::clone(&name),
                vec![LoadChunk::new(name.as_ref(), init)],
                LoadContext {
                    opts,
                    ..LoadContext::plain(None, permissions)
                },
            )?;
        }
        Ok(())
    }

    fn send_load(
        &self,
        name: Arc<str>,
        chunks: Vec<LoadChunk>,
        context: LoadContext,
    ) -> Result<(), PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::LoadSource {
                name,
                chunks,
                context,
                reply: reply_tx,
            })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?
    }

    /// Option specs declared by loaded plugins via `maki.api.register_options`,
    /// keyed by plugin name. Used by docgen.
    pub fn plugin_options(&self) -> Result<PluginOptionSpecs, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::CollectPluginOptions { reply: reply_tx })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)
    }

    /// Runs a source as the global `init.lua`.
    ///
    /// The one scope where `maki.pack.add` may declare packages, so it is its
    /// own method: deriving the privilege from a source name would let any
    /// caller reach it by spelling the name the right way.
    pub fn send_global_init_lua(
        &self,
        source: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        self.send_config_lua(source, ConfigScope::Global, plugin_dir)
    }

    /// Runs a source as a config chunk named after itself. It gets the
    /// read-only `maki.pack` table.
    pub fn send_run_init_lua(
        &self,
        source: String,
        source_name: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        self.send_config_lua(source, ConfigScope::Named(source_name), plugin_dir)
    }

    fn send_config_lua(
        &self,
        source: String,
        scope: ConfigScope,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::RunInitLua {
                source,
                scope,
                plugin_dir,
                reply: reply_tx,
            })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?
    }

    pub fn unload(&self, plugin: &str) -> Result<(), PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::ClearPlugin {
                plugin: Arc::from(plugin),
                reply: reply_tx,
            })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?;
        Ok(())
    }

    /// Runs a source with every permission granted, for tests and embedders.
    /// Lua arriving from disk must not: it goes through
    /// [`PluginHost::load_builtins`], [`PluginHost::load_packages`] or the
    /// `init.lua` path, each of which derives its grant from a manifest.
    pub fn load_source(&self, name: &str, source: &str) -> Result<(), PluginError> {
        self.load_source_with_opts(name, source, serde_json::Map::new())
    }

    pub fn load_source_with_opts(
        &self,
        name: &str,
        source: &str,
        opts: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), PluginError> {
        self.send_load(
            Arc::from(name),
            vec![LoadChunk::new(name, source)],
            LoadContext {
                opts: Arc::new(opts),
                ..LoadContext::plain(None, PluginPermissions::trusted())
            },
        )
    }

    pub fn load_source_with_permissions(
        &self,
        name: &str,
        source: &str,
        permissions: PluginPermissions,
    ) -> Result<(), PluginError> {
        self.send_load(
            Arc::from(name),
            vec![LoadChunk::new(name, source)],
            LoadContext::plain(None, permissions),
        )
    }

    pub fn load_plugin_file(&self, path: &Path) -> Result<(), PluginError> {
        let source = fs::read_to_string(path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let plugin_dir = path.parent().map(Path::to_path_buf);
        check_plugin_compatibility(USER_PLUGIN, plugin_dir.as_deref())?;
        let permissions = load_plugin_permissions(plugin_dir.as_deref());
        // Test-only path today. Once user plugin dirs exist: derive a real
        // plugin name, since the hardcoded "user" would collide across files,
        // pass the `plugins.<name>` opts through, and teach the
        // unknown-plugin guards about user plugin names.
        self.send_load(
            Arc::from(USER_PLUGIN),
            vec![LoadChunk::new(path.display().to_string(), source)],
            LoadContext::plain(plugin_dir, permissions),
        )
    }

    /// Packages declared by `maki.pack.add` in `init.lua`.
    ///
    /// Read after the init files have run, which is when the declared set is
    /// complete and before anything is installed.
    pub fn declared_packages(&self) -> Result<Vec<crate::api::pack::Declared>, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::CollectPackages { reply: reply_tx })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)
    }

    fn run_pack_loader(
        &self,
        declared: crate::api::pack::Declared,
        package: &DiscoveredPackage,
        permissions: PluginPermissions,
        opts: PluginOpts,
    ) -> Result<(), PluginError> {
        check_plugin_compatibility(&package.name, Some(&package.dir))?;
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::RunPackLoader {
                declared,
                context: LoadContext {
                    plugin_dir: Some(package.dir.clone()),
                    permissions,
                    opts,
                    revision_guard: package.revision_guard.clone(),
                    package: true,
                },
                reply: reply_tx,
            })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?
    }

    /// Loads one external package directory as a single owner.
    ///
    /// Every `plugin/*.lua` becomes a chunk, and the chunks share one
    /// environment, so what one file registers the next can use. The whole set
    /// commits or none of it does.
    pub fn load_package(
        &self,
        name: &str,
        dir: &Path,
        permissions: PluginPermissions,
        opts: PluginOpts,
    ) -> Result<(), PluginError> {
        self.load_package_with_guard(name, dir, permissions, opts, None)
    }

    fn load_package_with_guard(
        &self,
        name: &str,
        dir: &Path,
        permissions: PluginPermissions,
        opts: PluginOpts,
        revision_guard: Option<Arc<maki_pack::lock::Lock>>,
    ) -> Result<(), PluginError> {
        // Refused here and not only in discovery, because loading an owner
        // drops that owner's existing registrations first. A package named
        // after a bundled plugin would unload the builtin before its own
        // entrypoint ever ran, so every caller has to be gated, not just the
        // one that walks the site directory.
        if is_bundled(name) {
            return Err(PluginError::PackageNameConflict {
                name: name.to_owned(),
                path: dir.to_path_buf(),
            });
        }
        // Resolved once here, so the manifest, the entrypoints, and later
        // `require` calls all agree on one directory even if the path they came
        // from changes underneath us.
        let root = dir.canonicalize().map_err(|e| PluginError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        // Gated next to the bundled-name refusal and before any chunk is read,
        // so a package that outran this Maki registers nothing at all.
        check_plugin_compatibility(name, Some(&root))?;
        let files = package_entrypoints(&root)?;
        if files.is_empty() {
            return Err(PluginError::PackageEmpty {
                name: name.to_owned(),
                path: root,
            });
        }

        let mut chunks = Vec::with_capacity(files.len());
        for path in files {
            let source = fs::read_to_string(&path).map_err(|e| PluginError::Io {
                path: path.clone(),
                source: e,
            })?;
            chunks.push(LoadChunk::new(path.display().to_string(), source));
        }
        self.send_load(
            Arc::from(name),
            chunks,
            LoadContext {
                plugin_dir: Some(root),
                permissions,
                opts,
                revision_guard,
                package: true,
            },
        )
    }

    /// Refuses further `maki.packadd` calls, and returns anything the queue
    /// still holds.
    ///
    /// One call and not a read followed by a close, because a Lua task can
    /// record an activation between the two and closing would strand it.
    pub fn seal_pack_ops(&self) -> Result<Vec<crate::api::pack::PackOp>, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::SealPackOps { reply: reply_tx })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)
    }

    /// Takes the package operations Lua recorded, leaving the queue empty.
    ///
    /// Called by the host after the initiating task has exited, which keeps a
    /// load off the thread that requested it.
    fn take_pending_pack_ops(&self) -> Result<Vec<crate::api::pack::PackOp>, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::TakePackOps { reply: reply_tx })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)
    }

    /// Loads every package that should be loaded now: the `start/` ones, and
    /// the `opt/` ones that `maki.packadd` named.
    ///
    /// Activation names are collected here rather than acted on inside
    /// `packadd`, because a load waits on a reply from the runtime thread that
    /// `packadd` is called on. They are collected after each round as well as
    /// before the first, so a package that activates another one still has it
    /// loaded in this startup rather than the next.
    pub fn load_packages(
        &self,
        packages: &[DiscoveredPackage],
        config: &PluginsConfig,
    ) -> Vec<String> {
        self.load_declared_packages(packages, &[], config)
    }

    /// The names the plugin registered that maki's own permission defaults are
    /// keyed on. Taking such a name is allowed, and a drop-in replacement may
    /// want the builtin's rules, but the user has to be told which rules the
    /// plugin just inherited. One warning lists them all, because the TUI
    /// flashes a single warning and a per-tool one would drop the rest.
    fn permission_name_warning(&self, plugin: &str) -> Option<String> {
        let snapshot = self.registry.iter();
        let names: Vec<String> = snapshot
            .iter()
            .filter(|t| matches!(&t.source, ToolSource::Lua { plugin: p } if p.as_ref() == plugin))
            .filter(|t| carries_builtin_defaults(t.name()))
            .map(|t| format!("`{}`", t.name()))
            .collect();
        if names.is_empty() {
            return None;
        }
        Some(format!(
            "{plugin}: registered {}, so it {PERMISSION_NAME_WARNING}",
            names.join(", ")
        ))
    }

    /// As `load_packages`, with the declarations that may carry a custom
    /// loader. A package with no matching declaration loads its `plugin/*.lua`.
    pub fn load_declared_packages(
        &self,
        packages: &[DiscoveredPackage],
        declared: &[crate::api::pack::Declared],
        config: &PluginsConfig,
    ) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut loaded: Vec<&str> = Vec::new();
        let mut round: Vec<&DiscoveredPackage> = packages
            .iter()
            .filter(|pkg| pkg.eager && config.packages.iter().any(|n| n == &pkg.name))
            .collect();

        // A `loop` and not `while !round.is_empty()`: with no `start` package
        // installed the first round is empty, and the names `init.lua` already
        // recorded still have to be collected.
        loop {
            for pkg in round {
                let opts = config
                    .opts
                    .get(&pkg.name)
                    .cloned()
                    .map(Arc::new)
                    .unwrap_or_default();
                let permissions = crate::pack::effective_permissions(pkg);
                loaded.push(&pkg.name);
                let custom = declared
                    .iter()
                    .find(|declaration| declaration.spec.name == pkg.name)
                    .filter(|declaration| {
                        matches!(declaration.load, crate::api::pack::LoadMode::Custom(_))
                            && matches!(
                                &pkg.origin,
                                crate::pack::Origin::Fetched { src }
                                    if src == &declaration.spec.src
                            )
                    });
                let result = match custom {
                    Some(declaration) => {
                        self.run_pack_loader(declaration.clone(), pkg, permissions, opts)
                    }
                    None => self.load_package_with_guard(
                        &pkg.name,
                        &pkg.dir,
                        permissions,
                        opts,
                        pkg.revision_guard.clone(),
                    ),
                };
                match result {
                    Ok(()) => warnings.extend(self.permission_name_warning(&pkg.name)),
                    Err(e) if e.is_version_floor() => {
                        tracing::warn!(
                            package = %pkg.name,
                            path = %pkg.dir.display(),
                            error = %e,
                            "{SKIPPED_PLUGIN_WARNING}"
                        );
                        warnings.push(format!("{SKIPPED_PLUGIN_WARNING}: {e}"));
                    }
                    Err(e) => {
                        tracing::error!(
                            package = %pkg.name,
                            path = %pkg.dir.display(),
                            error = %e,
                            "failed to load package"
                        );
                        warnings.push(format!("{}: failed to load: {e}", pkg.name));
                    }
                }
            }

            let ops = match self.take_pending_pack_ops() {
                Ok(ops) => ops,
                Err(e) => {
                    warnings.push(format!("could not read package activations: {e}"));
                    break;
                }
            };
            round = Vec::new();
            for op in ops {
                let crate::api::pack::PackOp::Activate { name } = op;
                if loaded.contains(&name.as_str()) {
                    continue;
                }
                // Refused rather than loaded when the config disabled it, so
                // `packadd` cannot be a way around `plugins.<name>.enabled`.
                let found = packages
                    .iter()
                    .find(|pkg| pkg.name == name && config.packages.iter().any(|n| n == &pkg.name));
                match found {
                    Some(pkg) => round.push(pkg),
                    None => warnings.push(format!(
                        "packadd {name:?}: no package with that name is installed"
                    )),
                }
            }
            if round.is_empty() {
                break;
            }
        }
        // Nothing drains the queue after this, so `packadd` is closed rather
        // than left accepting names no one will read. Closing returns whatever
        // arrived since the last round, which is reported rather than dropped:
        // that request was going to be honoured a moment earlier.
        match self.seal_pack_ops() {
            Ok(leftover) => {
                for op in leftover {
                    let crate::api::pack::PackOp::Activate { name } = op;
                    warnings.push(format!(
                        "packadd {name:?}: arrived after the packages had loaded"
                    ));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not close the package activation queue");
            }
        }
        warnings
    }

    pub fn event_handle(&self) -> EventHandle {
        EventHandle {
            tx: self.inner.tx.clone(),
            prio_tx: self.inner.prio_tx.clone(),
        }
    }

    pub fn command_reader(&self) -> LuaCommandReader {
        self.inner.command_reader.clone()
    }

    pub fn keymap_reader(&self) -> KeymapReader {
        self.inner.keymap_reader.clone()
    }

    pub fn hint_reader(&self) -> HintReader {
        self.inner.hint_reader.clone()
    }

    pub fn completer_reader(&self) -> crate::api::util::command::CompleterReader {
        self.inner.completer_reader.clone()
    }

    pub fn ui_action_rx(&self) -> flume::Receiver<UiAction> {
        self.inner.ui_action_rx.clone()
    }

    /// The bit every `maki.ui` and `maki.fn` roundtrip consults. The event
    /// loop attaches while it drains [`Self::ui_action_rx`] and detaches
    /// before teardown runs `SessionEnd`, since that receiver is a clone and
    /// dropping it would leave a handler parked on a reply that never comes.
    pub fn ui_attachment(&self) -> UiAttachment {
        self.inner.ui_attachment.clone()
    }
}

#[derive(Clone)]
pub struct EventHandle {
    tx: flume::Sender<Request>,
    /// User-initiated requests bypass queued bulk work (session restores).
    prio_tx: flume::Sender<Request>,
}

impl EventHandle {
    pub(crate) fn from_tx(tx: flume::Sender<Request>) -> Self {
        Self {
            tx,
            prio_tx: flume::unbounded().0,
        }
    }

    #[doc(hidden)]
    pub fn disconnected_for_test() -> Self {
        Self::from_tx(flume::unbounded().0)
    }

    /// True when no runtime is draining requests. Production handles stay
    /// connected for the host's lifetime; the disconnected-for-test handle
    /// and a host whose thread has shut down both report true. Callers use
    /// this to skip async side effects (e.g. a restore-complete flip) that
    /// no live consumer would ever observe.
    pub fn is_disconnected(&self) -> bool {
        self.tx.is_disconnected() && self.prio_tx.is_disconnected()
    }

    /// Test probe sibling of `from_tx`: collapses both senders onto one
    /// channel so a `RequestProbe` sees every request, including the
    /// `prio_tx`-routed commands and keybind callbacks that `from_tx`
    /// would route to a disconnected channel.
    pub(crate) fn probed_for_test(shared: flume::Sender<Request>) -> Self {
        Self {
            tx: shared.clone(),
            prio_tx: shared,
        }
    }

    pub fn run_command(&self, plugin: Arc<str>, command: Arc<str>, args: String, depth: u8) {
        let _ = self.prio_tx.try_send(Request::RunCommand {
            plugin,
            command,
            args,
            depth,
        });
    }

    /// Fire-and-forget query to a registered input completer; the UI polls
    /// the returned receiver so a slow handler never blocks a keystroke.
    pub fn query_input_completer(
        &self,
        plugin: Arc<str>,
        name: Arc<str>,
        query: String,
    ) -> flume::Receiver<Option<Vec<crate::api::util::command::CompletionItem>>> {
        let (tx, rx) = flume::bounded(1);
        let _ = self.prio_tx.try_send(Request::QueryInputCompleter {
            plugin,
            name,
            query,
            reply: tx,
        });
        rx
    }

    pub fn collect_prompt_slots(&self) -> ResolvedSlots {
        let (tx, rx) = flume::bounded(1);
        let _ = self.tx.send(Request::CollectPromptSlots { reply: tx });
        rx.recv().unwrap_or_default()
    }

    pub fn package_context(&self) -> Result<crate::pack::PackContext, String> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send(Request::CollectPackageContext { reply: reply_tx })
            .map_err(|_| PACK_STATE_UNAVAILABLE.to_owned())?;
        let (declared, active) = reply_rx
            .recv()
            .map_err(|_| PACK_STATE_UNAVAILABLE.to_owned())?;
        let installed = crate::pack::installed_names()
            .ok_or_else(|| "could not read the package lockfile".to_owned())?;

        Ok(crate::pack::PackContext::new(declared, installed, active))
    }

    pub async fn collect_prompt_slots_async(&self) -> ResolvedSlots {
        let (tx, rx) = flume::bounded(1);
        let _ = self.tx.send(Request::CollectPromptSlots { reply: tx });
        rx.recv_async().await.unwrap_or_default()
    }

    pub fn request_restore(&self, item: RestoreItem, event_tx: maki_agent::EventSender) {
        let _ = self.tx.send(Request::RestoreToolAsync { item, event_tx });
    }

    /// `row` is the 1-based line in the tool's live buffer, 0 for clicks
    /// outside it (header line etc.).
    pub fn request_click(&self, tool_use_id: String, row: usize) {
        let _ = self.tx.send(Request::ClickTool {
            tool_use_id,
            row,
            fallback: None,
        });
    }

    /// Like [`Self::request_click`], but when the runtime no longer holds
    /// a live or warm handle for the tool it restores from `item` (whose
    /// `clicks` must already include `row`) and emits fresh snapshots on
    /// `event_tx`. Callers need no knowledge of the runtime's warm cache.
    pub fn request_click_with_fallback(
        &self,
        tool_use_id: String,
        row: usize,
        item: RestoreItem,
        event_tx: maki_agent::EventSender,
    ) {
        let _ = self.tx.send(Request::ClickTool {
            tool_use_id,
            row,
            fallback: Some(Box::new(ClickFallback { item, event_tx })),
        });
    }

    pub fn send_restore_complete(&self, flag: Arc<AtomicBool>) {
        let _ = self.tx.send(Request::RestoreComplete { flag });
    }

    /// Blocks until every restore item queued so far has finished; restores
    /// run as spawned tasks, and the `RestoreComplete` flag flips only once
    /// the whole batch has landed, making it the batch barrier.
    #[doc(hidden)]
    pub fn wait_restore_complete_for_test(&self) {
        const DEADLINE: Duration = Duration::from_secs(30);
        let flag = Arc::new(AtomicBool::new(true));
        self.send_restore_complete(Arc::clone(&flag));
        let start = std::time::Instant::now();
        while flag.load(Ordering::Relaxed) {
            assert!(start.elapsed() < DEADLINE, "restore batch never completed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn fire_autocmd(&self, event: &str, data: serde_json::Value) {
        let _ = self.tx.try_send(Request::FireAutocmd {
            event: event.to_owned(),
            data,
        });
    }

    /// Headless drivers install their own provider so `maki.session.read` has
    /// something to answer with instead of "no interactive UI attached". The UI
    /// leaves the slot empty and answers through its event loop, which owns the
    /// live session runtimes.
    pub fn install_session_snapshot(&self, provider: crate::api::session::SessionSnapshotFn) {
        let _ = self
            .tx
            .try_send(Request::InstallSessionSnapshot { provider });
    }

    /// Queue the kill of session-owned jobs and the `SessionEnd` dispatch,
    /// then return. Call from every session-end path so a Lua monitor can
    /// stay a plugin. Process exit wants [`Self::end_sessions_blocking`].
    ///
    /// Nothing waits here, so handlers get no deadline and the UI is still
    /// there to answer them.
    pub fn end_session(&self, session: MakiId, reason: SessionEndReason) {
        let _ = self.tx.try_send(Request::EndSession(EndSession {
            session,
            reason,
            wait: None,
        }));
    }

    /// [`Self::end_session`] for process exit: block until the handlers ran
    /// and the jobs were reaped, so the `Shutdown` that follows on the
    /// priority lane cannot skip ahead of them.
    ///
    /// Every session is queued first and the deadline is shared, so quitting
    /// with many tabs open costs one `SHUTDOWN_TIMEOUT`, not one per tab.
    pub fn end_sessions_blocking(
        &self,
        sessions: impl IntoIterator<Item = MakiId>,
        reason: SessionEndReason,
    ) {
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let waits: Vec<_> = sessions
            .into_iter()
            .filter_map(|session| {
                Some((session, self.send_end_session(session, reason, deadline)?))
            })
            .collect();
        for (session, reply_rx) in waits {
            let left = deadline.saturating_duration_since(Instant::now());
            if reply_rx.recv_timeout(left).is_err() {
                tracing::warn!(
                    session = %session,
                    "SessionEnd did not finish within timeout, continuing teardown"
                );
            }
        }
    }

    /// [`Self::end_sessions_blocking`] parked on a spare thread. ACP calls
    /// this from its executor, where blocking would freeze stdin for the
    /// whole grace period.
    pub async fn end_session_async(&self, session: MakiId, reason: SessionEndReason) {
        let handle = self.clone();
        smol::unblock(move || handle.end_sessions_blocking([session], reason)).await;
    }

    fn send_end_session(
        &self,
        session: MakiId,
        reason: SessionEndReason,
        deadline: Instant,
    ) -> Option<flume::Receiver<()>> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx
            .send(Request::EndSession(EndSession {
                session,
                reason,
                wait: Some((deadline, reply_tx)),
            }))
            .ok()?;
        Some(reply_rx)
    }

    pub fn run_keybind_callback(&self, id: u64) -> bool {
        self.prio_tx
            .try_send(Request::RunKeybindCallback { id })
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{LuaCommandInfo, LuaCommandWriter};
    use maki_agent::prompt::{PromptId, ResolvedSlots, Slot};
    use maki_agent::tools::ToolRegistry;
    use std::time::Instant;
    use test_case::test_case;

    /// Closing the queue and reading it are one message. A Lua task can record
    /// an activation between a separate read and close, and a close that threw
    /// the queue away would strand exactly the request that was about to be
    /// honoured.
    #[test]
    fn closing_the_activation_queue_hands_back_what_it_holds() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source("recorder", r#"maki.packadd("demo")"#)
            .expect("packadd is available to every plugin");

        let leftover = host.seal_pack_ops().expect("the host is running");

        assert_eq!(
            leftover,
            vec![crate::api::pack::PackOp::Activate {
                name: "demo".to_owned()
            }],
            "a recorded activation must come back, not be dropped"
        );
    }

    /// jit=true is exercised by the whole integration suite
    /// (`tests/plugin_host.rs` boots hosts via `new`); only the O1
    /// interpreter path needs its own coverage.
    #[test]
    fn with_jit_off_loads_builtins_and_registers_tools() {
        let reg = Arc::new(ToolRegistry::new());
        let mut host = PluginHost::with_jit(Arc::clone(&reg), false).unwrap();
        host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))
            .unwrap();
        assert!(reg.has("glob"));
    }

    /// The second call sends `Shutdown` on a sender that is already
    /// disconnected; it must swallow that error and keep rejecting work.
    #[test]
    fn begin_shutdown_rejects_later_loads_and_is_idempotent() {
        let mut host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.begin_shutdown();
        assert!(host.load_source("late", "return {}").is_err());
        host.begin_shutdown();
        assert!(host.load_source("later", "return {}").is_err());
    }

    /// Regression for the exit drain in `runtime::spawn`. An `EventHandle`
    /// clone keeps queued requests alive after the Lua thread exits, and
    /// dispatch prefers the priority lane, so a bulk request queued behind
    /// `Shutdown` is never served. Without the drain its reply sender lives
    /// forever and `collect_prompt_slots` blocks; with it, the call falls
    /// back to defaults right away.
    #[test]
    fn live_event_handle_does_not_hang_after_begin_shutdown() {
        let mut host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "hinted",
            r#"maki.api.register_prompt_hint({ slot = "tool_usage", content = "live" })"#,
        )
        .unwrap();
        let handle = host.event_handle();
        host.begin_shutdown();

        let slots = handle.collect_prompt_slots();
        assert!(
            contents(&slots, PromptId::System, Slot::ToolUsage).is_empty(),
            "dead host must yield defaults, not real slots"
        );

        drop(host);
        let slots = handle.collect_prompt_slots();
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
    }

    /// Load `src` as one plugin, collect resolved slots.
    /// Panics on failure; use `load_err` to inspect errors.
    fn slots_from(plugin: &str, src: &str) -> (PluginHost, ResolvedSlots) {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(plugin, src).unwrap();
        let slots = host.event_handle().collect_prompt_slots();
        (host, slots)
    }

    fn contents(slots: &ResolvedSlots, prompt: PromptId, slot: Slot) -> Vec<&str> {
        slots
            .get(prompt, slot)
            .iter()
            .map(|e| e.content.as_str())
            .collect()
    }

    #[test]
    fn command_writer_reader_pair_works() {
        let (writer, reader) = LuaCommandWriter::new();
        let snap = reader.load();
        assert_eq!(snap.commands.len(), 0);

        writer.publish(vec![LuaCommandInfo {
            name: Arc::from("/test"),
            description: Arc::from("desc"),
            plugin: Arc::from("p"),
            max_args: 0,
        }]);
        let snap = reader.load();
        assert_eq!(snap.commands.len(), 1);
        assert!(snap.generation > 0);
    }

    #[test]
    fn memory_builtin_registers_command() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::with_all_builtins(Arc::clone(&reg)).unwrap();
        let reader = host.command_reader();
        let snap = reader.load();
        let found = snap.commands.iter().any(|c| c.name.as_ref() == "/memory");
        assert!(
            found,
            "Expected /memory command, found: {:?}",
            snap.commands
                .iter()
                .map(|c| c.name.as_ref())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn run_command_sends_correct_request() {
        let (prio_tx, prio_rx) = flume::bounded(8);
        let (tx, _rx) = flume::bounded(8);
        let handle = EventHandle { tx, prio_tx };
        handle.run_command(
            Arc::from("myplugin"),
            Arc::from("/greet"),
            "world".into(),
            2,
        );
        let req = prio_rx.try_recv().unwrap();
        match req {
            Request::RunCommand {
                plugin,
                command,
                args,
                depth,
            } => {
                assert_eq!(plugin.as_ref(), "myplugin");
                assert_eq!(command.as_ref(), "/greet");
                assert_eq!(args, "world");
                assert_eq!(depth, 2);
            }
            _ => panic!("expected RunCommand"),
        }
    }

    #[test]
    fn multiple_plugins_register_independent_commands() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
        host.load_source(
            "plugin_a",
            r#"
            maki.api.register_command({
                name = "/alpha",
                description = "from a",
                handler = function() end,
            })
            "#,
        )
        .unwrap();
        host.load_source(
            "plugin_b",
            r#"
            maki.api.register_command({
                name = "/beta",
                description = "from b",
                handler = function() end,
            })
            "#,
        )
        .unwrap();

        let snap = host.command_reader().load();
        assert_eq!(snap.commands.len(), 2);
        let names: Vec<&str> = snap.commands.iter().map(|c| c.name.as_ref()).collect();
        assert!(names.contains(&"/alpha"));
        assert!(names.contains(&"/beta"));
    }

    #[test]
    fn register_command_adds_missing_leading_slash() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "noslash",
            r#"
            maki.api.register_command({
                name = "hello",
                description = "no slash",
                handler = function() end,
            })
            "#,
        )
        .unwrap();

        let snap = host.command_reader().load();
        assert_eq!(snap.commands.len(), 1);
        assert_eq!(snap.commands[0].name.as_ref(), "/hello");
    }

    #[test]
    fn command_reader_generation_increments_on_publish() {
        let (writer, reader) = LuaCommandWriter::new();
        assert_eq!(reader.load().generation, 0);
        writer.publish(vec![]);
        assert!(reader.load().generation > 0);
    }

    /// End-to-end: a plugin registers a keymap override, the override is published
    /// to the snapshot, EventHandle::run_keybind_callback dispatches the request,
    /// the runtime resolves the Function by id from the registry, and the callback
    /// executes with an observable side effect. This is the load-bearing path the
    /// dispatch reorder and the dead-host fallback rest on; unit tests only cover
    /// the layers in isolation.
    #[test]
    fn keybind_callback_runs_end_to_end() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "kb",
            r#"
            maki.keymap.set("n", "<C-g>", function()
                maki.api.register_command({
                    name = "/fired",
                    description = "callback ran",
                    handler = function() end,
                })
            end, { desc = "test override" })
            "#,
        )
        .unwrap();

        let snap = host.keymap_reader().load();
        assert_eq!(snap.entries.len(), 1, "override published to snapshot");
        let entry = &snap.entries[0];
        assert_eq!(entry.desc, "test override");
        assert!(
            host.command_reader().load().commands.is_empty(),
            "callback has not fired yet"
        );

        let handle = host.event_handle();
        handle.run_keybind_callback(entry.id);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let cmds = &host.command_reader().load().commands;
            if cmds.iter().any(|c| c.name.as_ref() == "/fired") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "keybind callback did not register /fired within 2s"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// `load_init_files_or_skip` is the single seam every entry point
    /// (TUI, index, acp, prompt) uses to honor `--no-plugins`. Verify both
    /// halves: the flag skips a broken init.lua, and absence runs it (so
    /// the skip path is not a tautology that hides a regression in the
    /// unconditional loader).
    #[test]
    fn load_init_files_or_skip_respects_flag() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".maki")).unwrap();
        fs::write(
            dir.path().join(".maki/init.lua"),
            "error('broken init lua must not run')",
        )
        .unwrap();

        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();

        let mut warnings = Vec::new();
        let skipped = host
            .load_init_files_or_skip(true, dir.path(), &mut warnings)
            .expect("no-plugins skips broken init.lua");
        assert!(
            skipped.is_none(),
            "--no-plugins must skip user init.lua entirely"
        );

        let ran = host.load_init_files_or_skip(false, dir.path(), &mut warnings);
        assert!(
            ran.is_err(),
            "without --no-plugins the broken init.lua must surface as an error"
        );
    }

    #[test]
    fn callback_string_lands_in_targeted_prompt_only() {
        let (_host, slots) = slots_from(
            "cb",
            r#"
            maki.api.register_prompt_hint({
                slot = "tool_usage",
                prompt = "general",
                content = function() return "from_cb" end,
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::General, Slot::ToolUsage),
            ["from_cb"]
        );
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
    }

    #[test]
    fn callback_returning_nil_contributes_nothing() {
        let (_host, slots) = slots_from(
            "nil_cb",
            r#"
            maki.api.register_prompt_hint({
                slot = "tool_usage",
                content = function() return nil end,
            })
            "#,
        );
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
    }

    /// A hint with no `prompt` is a default: it lands on every prompt that has the slot.
    #[test]
    fn static_no_prompt_lands_on_all_prompts_with_slot() {
        let (_host, slots) = slots_from(
            "static_hint",
            r#"
            maki.api.register_prompt_hint({
                slot = "efficient_tools",
                content = "index",
            })
            "#,
        );
        for &pid in PromptId::ALL {
            assert_eq!(contents(&slots, pid, Slot::EfficientTools), ["index"]);
        }
    }

    /// `conventions` lives on system and general but not research, so a default
    /// hint follows the slot and skips research.
    #[test]
    fn default_hint_skips_prompts_lacking_the_slot() {
        let (_host, slots) = slots_from(
            "conv",
            r#"
            maki.api.register_prompt_hint({
                slot = "conventions",
                content = "follow conventions",
            })
            "#,
        );
        for pid in [PromptId::System, PromptId::General] {
            assert_eq!(
                contents(&slots, pid, Slot::Conventions),
                ["follow conventions"]
            );
        }
        assert!(contents(&slots, PromptId::Research, Slot::Conventions).is_empty());
    }

    /// Targeting a prompt that does not have the slot quietly drops the hint.
    #[test]
    fn register_prompt_hint_rejects_incompatible_slot_prompt() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source(
            "drop",
            r#"
            maki.api.register_prompt_hint({
                slot = "after_instructions",
                prompt = "research",
                content = "never lands",
            })
            "#,
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("not available"));
    }

    #[test]
    fn prompt_list_targets_each_listed_prompt() {
        const CONTENT: &str = "shared";
        let (_host, slots) = slots_from(
            "list",
            r#"
            maki.api.register_prompt_hint({
                slot = "tool_usage",
                prompt = { "system", "research" },
                content = "shared",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::ToolUsage),
            [CONTENT]
        );
        assert_eq!(
            contents(&slots, PromptId::Research, Slot::ToolUsage),
            [CONTENT]
        );
        assert!(contents(&slots, PromptId::General, Slot::ToolUsage).is_empty());
    }

    #[test]
    fn multiple_plugins_sorted_by_plugin_name() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        for plugin in ["zzz", "aaa"] {
            host.load_source(
                plugin,
                r#"
                maki.api.register_prompt_hint({ slot = "tool_usage", content = "from_PLUGIN" })
                "#
                .replace("PLUGIN", plugin)
                .as_str(),
            )
            .unwrap();
        }
        let slots = host.event_handle().collect_prompt_slots();
        assert_eq!(
            contents(&slots, PromptId::System, Slot::ToolUsage),
            ["from_aaa", "from_zzz"],
            "entries must be ordered by plugin name"
        );
    }

    /// One plugin can register several hints; unloading it clears all of them.
    #[test]
    fn unload_clears_all_hints_from_plugin() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "multi",
            r#"
            maki.api.register_prompt_hint({ slot = "tool_usage", prompt = "system", content = "usage" })
            maki.api.register_prompt_hint({ slot = "conventions", prompt = "system", content = "conv" })
            "#,
        )
        .unwrap();
        let handle = host.event_handle();

        let slots = handle.collect_prompt_slots();
        assert_eq!(
            contents(&slots, PromptId::System, Slot::ToolUsage),
            ["usage"]
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Conventions),
            ["conv"]
        );

        host.unload("multi").unwrap();
        let slots = handle.collect_prompt_slots();
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
        assert!(contents(&slots, PromptId::System, Slot::Conventions).is_empty());
    }

    #[test_case(r#"{ slot = "nonexistent", content = "x" }"# ; "invalid_slot")]
    #[test_case(r#"{ slot = "tool_usage", content = "x", prompt = "nope" }"# ; "invalid_prompt")]
    #[test_case(r#"{ slot = "tool_usage", content = "x", prompt = { "system", "bogus" } }"# ; "invalid_prompt_in_list")]
    #[test_case(r#"{ slot = "tool_usage" }"# ; "missing_content")]
    #[test_case(r#"{ content = "x" }"# ; "missing_slot")]
    #[test_case(r#"{ slot = "tool_usage", content = 42 }"# ; "content_wrong_type")]
    #[test_case(r#"{ slot = "tool_usage", content = "x", prompt = 42 }"# ; "prompt_wrong_type")]
    fn invalid_hint_spec_is_rejected(spec: &str) {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let src = format!("maki.api.register_prompt_hint({spec})");
        assert!(host.load_source("bad", &src).is_err());
    }

    #[test]
    fn identity_slot_lands_on_system_only() {
        let (_host, slots) = slots_from(
            "id",
            r#"
            maki.api.set_prompt({
                slot = "identity",
                content = "Custom identity",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Identity),
            ["Custom identity"]
        );
        assert!(contents(&slots, PromptId::Research, Slot::Identity).is_empty());
        assert!(contents(&slots, PromptId::General, Slot::Identity).is_empty());
    }

    #[test]
    fn tone_slot_lands_on_system_only() {
        let (_host, slots) = slots_from(
            "tone",
            r#"
            maki.api.set_prompt({
                slot = "tone",
                content = "Custom tone",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Tone),
            ["Custom tone"]
        );
        assert!(contents(&slots, PromptId::Research, Slot::Tone).is_empty());
        assert!(contents(&slots, PromptId::General, Slot::Tone).is_empty());
    }

    #[test]
    fn singleton_last_wins_across_plugins() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "aaa",
            r#"maki.api.set_prompt({ slot = "identity", content = "AAA" })"#,
        )
        .unwrap();
        host.load_source(
            "zzz",
            r#"maki.api.set_prompt({ slot = "identity", content = "ZZZ" })"#,
        )
        .unwrap();
        let slots = host.event_handle().collect_prompt_slots();
        let entries = slots.get(PromptId::System, Slot::Identity);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries.last().unwrap().content, "ZZZ");
    }

    #[test]
    fn content_required() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source("bad", r#"maki.api.set_prompt({ slot = "identity" })"#);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("'content' is required"));
    }

    #[test]
    fn set_prompt_sets_identity() {
        let (_host, slots) = slots_from(
            "setter",
            r#"
            maki.api.set_prompt({
                slot = "identity",
                content = "New identity",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Identity),
            ["New identity"]
        );
    }

    #[test]
    fn set_prompt_explicit_system_prompt() {
        let (_host, slots) = slots_from(
            "setter",
            r#"
            maki.api.set_prompt({
                slot = "identity",
                prompt = "system",
                content = "Explicit identity",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Identity),
            ["Explicit identity"]
        );
    }

    #[test]
    fn prompt_field_targets_specific_prompt() {
        let (_host, slots) = slots_from(
            "targeter",
            r#"
            maki.api.register_prompt_hint({
                slot = "tool_usage",
                prompt = "general",
                content = "General hint",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::General, Slot::ToolUsage),
            ["General hint"]
        );
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
    }

    #[test]
    fn set_prompt_invalid_prompt_rejected() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source(
            "bad",
            r#"maki.api.set_prompt({ slot = "identity", prompt = "nope", content = "x" })"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn set_prompt_and_register_prompt_hint_coexist() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "hint",
            r#"maki.api.register_prompt_hint({ slot = "tool_usage", content = "HINT" })"#,
        )
        .unwrap();
        host.load_source(
            "setter",
            r#"maki.api.set_prompt({ slot = "identity", content = "SET" })"#,
        )
        .unwrap();
        let slots = host.event_handle().collect_prompt_slots();
        assert_eq!(
            contents(&slots, PromptId::System, Slot::ToolUsage),
            ["HINT"]
        );
        assert_eq!(contents(&slots, PromptId::System, Slot::Identity), ["SET"]);
    }

    #[test]
    fn set_prompt_rejects_aggregate_slot() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source(
            "bad",
            r#"maki.api.set_prompt({ slot = "tool_usage", content = "nope" })"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn set_prompt_rejects_incompatible_slot_prompt() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source(
            "bad",
            r#"maki.api.set_prompt({ slot = "identity", prompt = "research", content = "x" })"#,
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("not available"));
    }

    #[test]
    fn empty_prompt_table_is_rejected() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source(
            "bad",
            r#"maki.api.set_prompt({ slot = "identity", prompt = {}, content = "x" })"#,
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("no sequence entries"));
    }

    #[test]
    fn content_must_not_be_empty() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        let r = host.load_source(
            "bad",
            r#"maki.api.set_prompt({ slot = "identity", content = "" })"#,
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn set_prompt_with_callback() {
        let (_host, slots) = slots_from(
            "setter_cb",
            r#"
            maki.api.set_prompt({
                slot = "identity",
                content = function() return "Dyn identity" end,
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Identity),
            ["Dyn identity"]
        );
    }
}

/// Holds every bundled `plugin.toml` to what its plugin actually does: the
/// guarded `maki.*` calls in its lua, and the permissions its tools expose to
/// the model. The guard map is read out of the `lua_fn` attributes the docs
/// already record, so moving a function under a different guard shows up here
/// without anyone editing a table.
#[cfg(test)]
mod bundled_manifests {
    use std::collections::{BTreeMap, BTreeSet};

    use include_dir::{Dir, DirEntry, File};
    use maki_agent::tools::{ToolRegistry, ToolSource};
    use maki_config::{DEFAULT_BUILTINS, PluginsConfig};

    use super::{Arc, BUNDLED_PLUGINS, HashMap, PluginHost, bundled_permissions, lib_dir};
    use crate::docs::{DocKind, api_docs};
    use crate::plugin_permissions::Permission;

    const TEST_DIR: &str = "tests";
    const LUA_EXT: &str = "lua";
    const REQUIRE_CALL: &str = "require(";

    /// Every guarded `maki.*` function under the dotted name lua calls it by.
    fn guarded_calls() -> Vec<(String, Permission)> {
        api_docs()
            .into_iter()
            .filter(|module| module.kind == DocKind::Table)
            .flat_map(|module| {
                module.fns.iter().filter_map(move |func| {
                    let permission = Permission::from_key(func.guard?)?;
                    Some((format!("{}.{}", module.name, func.name), permission))
                })
            })
            .collect()
    }

    /// Read off a real load rather than off the source text, because
    /// `register_tool` refuses a permission the plugin lacks, which makes
    /// shipping the tool itself a use of it.
    fn tool_permissions() -> BTreeMap<String, BTreeSet<Permission>> {
        let registry = Arc::new(ToolRegistry::new());
        let mut host = PluginHost::new(Arc::clone(&registry)).expect("host starts");
        host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))
            .expect("every bundled plugin loads");

        let mut out: BTreeMap<String, BTreeSet<Permission>> = BTreeMap::new();
        for tool in registry.iter().iter() {
            let (ToolSource::Lua { plugin }, Some(permission)) =
                (&tool.source, tool.tool.required_permission())
            else {
                continue;
            };
            out.entry(plugin.to_string())
                .or_default()
                .insert(permission);
        }
        out
    }

    /// Specs, not runtime code. What a test calls must not buy the plugin a
    /// permission its handlers never use.
    fn runtime_lua(file: &'static File<'static>) -> Option<&'static str> {
        let path = file.path();
        if path.extension()? != LUA_EXT || path.components().any(|p| p.as_os_str() == TEST_DIR) {
            return None;
        }
        file.contents_utf8()
    }

    fn collect_runtime_lua(dir: &'static Dir<'static>, out: &mut Vec<&'static str>) {
        for entry in dir.entries() {
            match entry {
                DirEntry::Dir(sub) => collect_runtime_lua(sub, out),
                DirEntry::File(file) => out.extend(runtime_lua(file)),
            }
        }
    }

    fn required_module_names(source: &'static str) -> impl Iterator<Item = &'static str> {
        source.match_indices(REQUIRE_CALL).filter_map(|(start, _)| {
            let rest = &source[start + REQUIRE_CALL.len()..];
            let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            let name = &rest[quote.len_utf8()..];
            name.find(quote).map(|end| &name[..end])
        })
    }

    /// A guard resolves against the calling plugin, so a `lib` helper spends
    /// the caller's permissions and counts as the caller's usage. `lib` is the
    /// only plugin another one reaches into: anything else a `require` names is
    /// the plugin's own file, already collected, or a virtual module such as
    /// `plugin_dev`. A plugin's own directory is taken whole rather than walked
    /// from its entrypoint, because `index` builds its language module names at
    /// runtime.
    fn runtime_sources(dir: &'static Dir<'static>) -> Vec<&'static str> {
        let mut sources = Vec::new();
        collect_runtime_lua(dir, &mut sources);
        let mut seen = BTreeSet::new();
        let mut next = 0;
        while let Some(source) = sources.get(next).copied() {
            next += 1;
            let reached: Vec<&'static str> = required_module_names(source)
                .filter(|modname| seen.insert(*modname))
                .filter_map(|modname| {
                    lib_dir().get_file(format!("{}.{LUA_EXT}", modname.replace('.', "/")))
                })
                .filter_map(runtime_lua)
                .collect();
            sources.extend(reached);
        }
        sources
    }

    /// Whole-word only, so `maki.fs.read` is not reported for every
    /// `maki.fs.read_bytes`.
    fn calls(source: &str, name: &str) -> bool {
        source.match_indices(name).any(|(at, _)| {
            !source[at + name.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
    }

    #[test]
    fn bundled_manifests_match_the_permissions_their_plugin_uses() {
        let tools = tool_permissions();
        let guarded = guarded_calls();
        let mut drift = Vec::new();
        // `lib` is the one bundled directory that never loads on its own, so it
        // ships no manifest and its modules answer to whoever requires them.
        for plugin in BUNDLED_PLUGINS
            .iter()
            .filter(|p| DEFAULT_BUILTINS.contains(&p.name))
        {
            let declared = bundled_permissions(plugin).expect("every builtin ships a plugin.toml");
            // Each permission paired with the usage demanding it, so a failure
            // points at something to go look at.
            let mut needed: BTreeMap<Permission, String> = tools
                .get(plugin.name)
                .into_iter()
                .flatten()
                .map(|permission| (*permission, format!("a tool exposing '{permission}'")))
                .collect();
            for source in runtime_sources(&plugin.dir) {
                for (name, permission) in &guarded {
                    if calls(source, name) {
                        needed.entry(*permission).or_insert_with(|| name.clone());
                    }
                }
            }

            let manifest = format!("plugins/{}/plugin.toml", plugin.name);
            for &permission in Permission::ALL {
                match (declared.is_allowed(permission), needed.get(&permission)) {
                    (false, Some(usage)) => drift.push(format!(
                        "{manifest}: {usage} needs '{permission}', grant it or drop the usage"
                    )),
                    (true, None) => drift.push(format!(
                        "{manifest}: declares '{permission}' but nothing its runtime lua reaches needs it, remove it"
                    )),
                    _ => {}
                }
            }
        }
        assert!(drift.is_empty(), "plugin.toml drift:\n{}", drift.join("\n"));
    }
}
