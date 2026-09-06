use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use etcetera::base_strategy::BaseStrategy;

use crate::StateDir;

/// The directory name Maki uses for itself, in a home directory (`~/.maki`,
/// the legacy layout) and in a project (`<project>/.maki`) alike.
const MAKI_DIR: &str = ".maki";
const APP_NAME: &str = "maki";

/// The state dir is closed by default, so this names the parts of it that stay
/// open. The memory plugin keeps its notes under the first one, the skill
/// plugin writes the Lua API reference under the second, and plan mode has
/// the agent write the plan itself under the third, with the `write` tool.
/// Public so a caller can point at one without spelling it again.
///
/// Each name here is a feature that stops working the day it drops off the
/// list, and it stops quietly: the agent gets a refusal it cannot act on
/// rather than a crash somebody sees. So each one also has a test that asks
/// the guard about the path its feature really uses.
pub const OPEN_STATE_SUBTREES: [&str; 3] = ["projects", "docs", "plans"];
/// Where packages are checked out, under the data dir. Public so the code
/// that does the checking out and the rule that covers it cannot drift apart.
pub const SITE_DIR: &str = "site";
const CONFIG_ENTRY_POINT: &str = "init.lua";
const LUA_MODULE_DIR: &str = "lua";
const PLUGIN_MANIFEST: &str = "plugin.toml";
const PERMISSIONS_FILE: &str = "permissions.toml";
const PROVIDERS_DIR: &str = "providers";
const ENV_FILE: &str = ".env";
const MCP_CONFIG: &str = "mcp.toml";
const PROVIDERS_CONFIG: &str = "providers.toml";
/// The Lua Maki runs at startup: the entry point and the modules it
/// `require`s. Not read-only, because writing a plugin is a thing users ask
/// Maki for and the docs walk them through it. It is still a write that
/// decides what the next start runs, so the permission layer asks first, and
/// the user can answer once or say always.
const STARTUP_LUA: [&str; 2] = [CONFIG_ENTRY_POINT, LUA_MODULE_DIR];
/// What a config dir holds that decides what Maki does on the next start and
/// has no workflow where the agent writes it.
///
/// `plugin.toml` grants a plugin its permissions, and `permissions.toml` says
/// which tools the agent may use without asking, so a `default = "allow"` in
/// there ungates `bash` and takes the rest of this guard with it. Maki writes
/// `permissions.toml` itself when the user answers "always", through
/// `append_permission_rule`, which goes straight to `atomic_write` and never
/// through `maki.fs`. Every file under `providers` is a program Maki runs at
/// startup and on every provider create, and its output picks the base URL
/// and the auth headers for all model traffic.
///
/// The last three do not look like code, and each one is still at least as
/// powerful as something above it:
///
/// `.env` is read into the process environment, and every key it names that
/// the environment does not have yet gets set. `HOME` is such a key, and
/// `HOME` is where Maki looks for everything else, so one line in there picks
/// the config dir, and with it the `init.lua` and the `permissions.toml` of
/// the next start. `LD_PRELOAD` is another such key, and that one lands in
/// every child process Maki starts.
///
/// `mcp.toml` names programs together with their arguments, and Maki starts
/// every stdio server in it on the next start.
///
/// `providers.toml` sets the base URL, the headers and the API key per
/// provider, so one write sends all model traffic and the credentials that
/// travel with it to somebody else's endpoint, quietly and for as long as
/// nobody reads the file.
const CONFIG_READ_ONLY: [&str; 6] = [
    PLUGIN_MANIFEST,
    PERMISSIONS_FILE,
    PROVIDERS_DIR,
    ENV_FILE,
    MCP_CONFIG,
    PROVIDERS_CONFIG,
];
/// What a `.maki` directory holds that Maki obeys, matched by the shape of
/// the path rather than by a place on disk, so a project's own `.maki` is
/// covered wherever the project happens to live.
///
/// A folder the user trusts gets to say what Maki runs in it. That is the
/// user's decision, made once, and a trust record keeps file names rather
/// than file content, so nothing revisits it when the content changes. An
/// agent that could rewrite `<project>/.maki/permissions.toml` would grant
/// itself `bash` for the next start.
///
/// `.env` and `mcp.toml` are here for the reasons written above
/// `CONFIG_READ_ONLY`, which hold word for word in a project: the first one
/// lands in Maki's environment and from there in every child process, and the
/// second one names programs Maki starts on the next start.
///
/// Invariant: every name folder trust asks the user about is either here or
/// in `STARTUP_LUA`. Trust is a decision about the files that are in a
/// folder, so a name trust gates and both lists miss is a file the agent can
/// rewrite after the user has already said yes, with nothing left to ask
/// again. The names in `STARTUP_LUA` are asked about one write at a time
/// instead, by the permission layer. That other list lives with the trust
/// feature, in `maki-config`, because `maki-storage` knows nothing about
/// trust and needs nothing: the answer here is the same in a trusted folder
/// and in an untrusted one. Neither list can be derived from the other, so
/// changing one means changing the other by hand.
const MAKI_DIR_READ_ONLY: [&str; 4] = [PLUGIN_MANIFEST, PERMISSIONS_FILE, ENV_FILE, MCP_CONFIG];
/// The rest of what a user keeps in a config dir, for the layouts where that
/// dir is also Maki's state dir. `CONFIG_READ_ONLY` covers the entries above,
/// which stay readable in their own right, and `STARTUP_LUA` is opened
/// alongside this list, since a config dir that doubles as the state dir must
/// not be the one place where writing a plugin is refused outright.
///
/// Every name here has a reader in Maki, and that reader is where the name
/// came from: `config.toml` (`src/cmd/tui.rs`), `pack-lock.json`
/// (`maki-lua::pack::lockfile_path`), `AGENTS.md` (`maki-config` and
/// `maki-agent::agent::instructions`), `skills` (the skill plugin),
/// `commands` (`maki-agent::command`) and `themes` (`maki-ui::theme`).
///
/// Leaving a name out costs a user a working feature and gets reported.
/// Putting a name in that Maki also uses for its own state would be the real
/// mistake, so nothing goes in here without a reader that wants user content.
const CONFIG_CONTENT: [&str; 6] = [
    "config.toml",
    "pack-lock.json",
    "AGENTS.md",
    "skills",
    "commands",
    "themes",
];

static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
static STRATEGY: OnceLock<Option<Paths>> = OnceLock::new();
static PROTECTION_RULES: OnceLock<Guard> = OnceLock::new();

/// `None` means the path is open, which is how an allowed subtree cancels the
/// wider rule it sits inside.
type ProtectionRule = (PathBuf, Option<Protection>);

/// The rules and, just as important, what a path that none of them name gets.
struct Rules {
    ordered: Vec<ProtectionRule>,
    /// Normally `None`, because a path outside every rule is an ordinary file.
    /// When Maki cannot tell where its own state lives, an unnamed path could
    /// be that state, so it is refused instead.
    unmatched: Option<Protection>,
}

/// How far a caller that is not Maki itself may go with one of Maki's own
/// files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protection {
    /// Everything Maki stores about itself: tokens, sessions, caches, and
    /// whatever lands there next. Nobody outside Maki has a reason to read it,
    /// let alone write it.
    NoAccess,
    /// Reading is legitimate and load-bearing: config dirs and package
    /// checkouts hold plugins and skills that read each other. A write is a
    /// widened permission, or fresh code, the next time Maki starts.
    NoWrite,
}

struct Paths {
    config: PathBuf,
    data: PathBuf,
    state: PathBuf,
    logs: PathBuf,
    cache: PathBuf,
    xdg_config: PathBuf,
    /// Where user config is looked for, worked out once with the rest of the
    /// layout so that every caller gets the same answer all process long.
    config_search: Vec<PathBuf>,
}

/// Lexical path normalization that never hits the filesystem.
///
/// Returns an absolute path with `..` and `.` components resolved, but without
/// calling `canonicalize`. This means no `\\?\` prefix on Windows and no symlink
/// resolution. Use this for display, logging, and scope matching.
pub fn normalize_path(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    normalize_abs_path(&abs)
}

fn normalize_abs_path(abs: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in abs.components() {
        match component {
            Component::ParentDir => {
                // Only pop if the trailing component is a normal directory,
                // never a root or prefix.
                if let Some(Component::Normal(_)) = result.components().next_back() {
                    result.pop();
                }
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// Canonicalize a path (resolving symlinks) but strip the `\\?\` prefix
/// that Windows adds. Falls back to `normalize_path` if the path does not
/// exist yet.
///
/// Contract: the input is a "normal" path (no `\\?\` prefix). The output is
/// always display-friendly: no `\\?\`, no `..` components. On Windows UNC
/// paths (`\\?\UNC\server\share`), the result is `\\server\share`.
///
/// The result is for display, logging, and scope matching only. Do not pass
/// it to Win32 filesystem APIs if the path exceeds 260 characters (the
/// `\\?\` prefix is what bypasses that limit).
pub fn canonicalize_clean(path: &Path) -> PathBuf {
    match fs::canonicalize(path) {
        Ok(canon) => strip_windows_extended_prefix(&canon),
        Err(_) => normalize_path(path),
    }
}

/// Canonicalize a path by resolving each component left-to-right through
/// the filesystem.
///
/// At each step, the accumulated path is canonicalized so that symlinks
/// are resolved *before* a subsequent `..` component can traverse through
/// them. For non-existent tail components, falls back to lexical append.
///
/// This is the correct canonicalization for security-sensitive path checks
/// (boundary verification, scope matching) where symlink escapes matter.
/// Unlike `canonicalize_clean`, this never resolves `..` lexically when
/// a symlink is in play.
///
/// Returns `None` if the root/prefix portion of the path cannot be resolved.
pub fn incremental_canonicalize(path: &Path) -> Option<PathBuf> {
    let mut current = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                current.push(component);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                let next = current.join("..");
                if let Ok(canon) = next.canonicalize() {
                    current = strip_windows_extended_prefix(&canon);
                } else if let Some(Component::Normal(_)) = current.components().next_back() {
                    current.pop();
                }
            }
            Component::Normal(name) => {
                let next = current.join(name);
                match next.canonicalize() {
                    Ok(canon) => current = strip_windows_extended_prefix(&canon),
                    Err(_) => {
                        // `current` is already canonical from a prior iteration,
                        // so we can append the non-existent tail directly without
                        // re-resolving the parent.
                        current = next;
                    }
                }
            }
        }
    }

    if current.as_os_str().is_empty() {
        None
    } else {
        Some(current)
    }
}

/// Resolve a leading `~`. The one answer to what a tilde means, because a
/// spelling one layer expands and another does not is two names for one file.
pub fn expand_tilde(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), home()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

/// The identity of a file, independent of how a path was spelled: relative or
/// absolute, with `..` or not, through a symlink or not, under `~` or spelled
/// out, existing or not yet.
///
/// Over-resolving is safe here; under-resolving is the bug, because two keys
/// for one file mean two locks for one file, or a staleness check that looks
/// up an entry nobody wrote.
pub fn canonical_key(path: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    let abs = std::path::absolute(&expanded).unwrap_or(expanded);
    incremental_canonicalize(&abs).unwrap_or_else(|| normalize_path(&abs))
}

/// Strip the `\\?\` prefix that Windows `canonicalize` adds, using the
/// Rust `Prefix` enum for correct WTF-8 handling (no `.to_str()` lossy
/// conversion).
///
/// `\\?\C:\foo` becomes `C:\foo`.
/// `\\?\UNC\server\share\dir` becomes `\\server\share\dir`.
///
/// **Contract**: the result is for display, logging, and scope matching only.
/// Do not pass it to Win32 filesystem APIs if the path exceeds 260 characters
/// (the `\\?\` prefix is what bypasses that limit).
#[cfg(windows)]
fn strip_windows_extended_prefix(canon: &Path) -> PathBuf {
    use std::path::Prefix;

    let mut components = canon.components();
    let Some(Component::Prefix(pfx)) = components.next() else {
        return canon.to_path_buf();
    };
    let rest = components.as_path();
    match pfx.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:", drive as char)).join(rest),
        Prefix::VerbatimUNC(server, share) => {
            let mut base = PathBuf::from(r"\\");
            base.push(server);
            base.push(share);
            base.join(rest)
        }
        _ => canon.to_path_buf(),
    }
}

#[cfg(not(windows))]
fn strip_windows_extended_prefix(canon: &Path) -> PathBuf {
    canon.to_path_buf()
}

fn state_logs(s: &impl BaseStrategy, fallback: &Path) -> (PathBuf, PathBuf) {
    let state_base = s.state_dir();
    let state = state_base
        .as_ref()
        .map(|d| d.join(APP_NAME))
        .unwrap_or_else(|| fallback.to_path_buf());
    let logs = state_base
        .as_ref()
        .and_then(|d| d.parent().map(|p| p.join("logs").join(APP_NAME)))
        .unwrap_or_else(|| fallback.to_path_buf());
    (state, logs)
}

/// Work out the whole layout once and keep it.
///
/// Everything below reads this snapshot, so the answer to "where is my
/// config" cannot change while Maki runs. It matters because Maki loads
/// `.env` files into its own environment at startup, and a `HOME` in one of
/// them would otherwise move the config dir, the state dir and the rules that
/// protect them, in the middle of the process that is reading them. See
/// `freeze`.
fn resolve() -> Option<&'static Paths> {
    STRATEGY
        .get_or_init(|| {
            let s = etcetera::choose_base_strategy().ok()?;
            let fallback_dir = home().map(|h| h.join(MAKI_DIR)).filter(|d| d.is_dir());
            let xdg_config = s.config_dir().join(APP_NAME);
            let (data, cache, config) = match &fallback_dir {
                Some(dir) => (dir.clone(), dir.clone(), dir.clone()),
                None => (
                    s.data_dir().join(APP_NAME),
                    s.cache_dir().join(APP_NAME),
                    xdg_config.clone(),
                ),
            };
            let (state, logs) = match &fallback_dir {
                Some(dir) => (dir.clone(), dir.clone()),
                None => state_logs(&s, &data),
            };
            let config_search = config_search_dirs_from(home().as_deref(), Some(&xdg_config));
            Some(Paths {
                config,
                data,
                state,
                logs,
                cache,
                xdg_config,
                config_search,
            })
        })
        .as_ref()
}

/// Pin Maki's own directories for the rest of the process.
///
/// Call this before loading any `.env` file, project or global. Those files
/// are read into the process environment, `HOME` is a key like any other, and
/// `HOME` is what `etcetera` answers questions with. Resolving first means an
/// `.env` can still set `HOME` for the tools Maki starts, and cannot move
/// Maki's own config, state or protection rules out from under it.
///
/// The ordering used to hold by accident: loading `.env` looked up the global
/// file first, which resolved the layout on the way. Nobody would guess that,
/// and a small change to the lookup would have quietly undone it, so the
/// first thing that touches the environment now says so out loud.
pub fn freeze() {
    let _ = resolve();
    let _ = guard();
}

fn err() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "cannot determine base directories",
    )
}

fn ensure(path: &Path) -> Result<PathBuf, std::io::Error> {
    fs::create_dir_all(path)?;
    Ok(path.to_path_buf())
}

pub fn config_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.config)
}

pub fn xdg_config_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.xdg_config)
}

pub fn data_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.data)
}

pub fn state_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.state)
}

pub fn logs_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.logs)
}

pub fn cache_dir() -> Result<PathBuf, std::io::Error> {
    let p = resolve().ok_or_else(err)?;
    ensure(&p.cache)
}

pub struct XdgPaths {
    pub config: PathBuf,
    pub state: PathBuf,
    pub logs: PathBuf,
}

pub fn xdg_paths() -> Result<XdgPaths, std::io::Error> {
    let s = etcetera::choose_base_strategy().map_err(|_| err())?;
    let data = s.data_dir().join(APP_NAME);
    let (state, logs) = state_logs(&s, &data);
    Ok(XdgPaths {
        config: s.config_dir().join(APP_NAME),
        state,
        logs,
    })
}

/// The home directory as it was when Maki started.
///
/// Frozen because `HOME` is where every other answer here comes from, and
/// Maki sets environment variables on itself while it starts up. Two answers
/// to this question in one process would mean two layouts, and the guard
/// would be protecting the first one while the loaders read the second.
pub fn home() -> Option<PathBuf> {
    HOME.get_or_init(|| etcetera::home_dir().ok()).clone()
}

pub fn legacy_home_dir() -> Option<PathBuf> {
    home().map(|h| h.join(MAKI_DIR)).filter(|d| d.is_dir())
}

/// Where to look for user config, best match first. Writes still go to
/// `config_dir()`.
///
/// The two are not the same: `config_dir()` collapses to `~/.maki` the moment
/// that directory exists, so anything that reads it alone goes blind to
/// `~/.config/maki`, which is where the docs tell people to put their files.
///
/// Listing candidates creates none of them. Callers that write make their own
/// directory, and a caller that takes the first one that exists would get a
/// different answer if merely asking had brought one into being.
///
/// The list is part of the frozen layout, so `load_init_files`,
/// `load_permissions` and `find_config_path` all read the same one from the
/// first call to the last. An empty list means Maki could not work out where
/// its directories are, which is the same state that makes the guard refuse
/// every path.
pub fn config_search_dirs() -> Vec<PathBuf> {
    resolve()
        .map(|p| p.config_search.clone())
        .unwrap_or_default()
}

pub fn find_config_path(name: &str) -> Option<PathBuf> {
    config_search_dirs()
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|path| path.exists())
}

/// How `path` is protected, or `None` for an ordinary path that Maki's
/// filesystem APIs may hand out freely.
///
/// Resolution goes through `canonical_key`, so neither `..` nor a symlink
/// spells a way around a rule, and a file that does not exist yet is covered
/// before it is created. The rule set is built once: `is_unreachable` runs on
/// every entry of every walk.
///
/// This is worth something only where the `bash` tool is gated. An agent that
/// can run shell commands reads these files through the shell, and no check
/// inside the process can stop that, only an OS sandbox can.
pub fn protected(path: &Path) -> Option<Protection> {
    guard().protected(path)
}

/// `protected` for a caller that already resolved the path to a canonical key.
///
/// The walkers use this because resolving a key is the expensive half and they
/// can do it for a whole subtree at once. Handing in a key that is not
/// canonical asks the wrong question, so resolve with `canonical_key` or an
/// equivalent that a symlink cannot fool.
pub fn protected_key(key: &Path) -> Option<Protection> {
    guard().protected_key(key)
}

/// Whether Maki's own directory walkers must skip `path` entirely: never
/// descend into it, never read it, never name it in a result.
pub fn is_unreachable(path: &Path) -> bool {
    guard().is_unreachable(path)
}

/// Whether `path` is Lua that Maki runs when it starts: `init.lua` or a
/// module under `lua/`, in a config dir or in any `.maki` directory.
///
/// Writing one is a normal thing to ask Maki for, so nothing here refuses it.
/// The answer is for the permission layer, which uses it to ask the user
/// first rather than counting the file as ordinary project content.
pub fn is_startup_lua(path: &Path) -> bool {
    guard().is_startup_lua(path)
}

/// Whether removing `path` and everything under it would take one of Maki's
/// own files with it.
///
/// Asking `protected` about the path alone is not enough for a recursive
/// removal, because every rule names a root and a caller standing above that
/// root never has to name a protected file to destroy it. Removing a config
/// dir takes `permissions.toml` with it, removing the parent of the state dir
/// takes the credentials, and neither of those paths is protected in its own
/// right.
pub fn contains_protected(path: &Path) -> bool {
    guard().contains_protected(path)
}

/// The rule set the answers above come from, built once from the frozen
/// layout, so it protects the same directories the loaders read.
pub fn guard() -> &'static Guard {
    PROTECTION_RULES.get_or_init(|| {
        let paths = resolve();
        if paths.is_none() {
            tracing::warn!(
                guard = "degraded",
                reason = "no base directories",
                "cannot tell where Maki's own files live, so every path is refused"
            );
        }
        Guard::for_layout(&Layout {
            state: paths.map(|p| p.state.as_path()),
            data: paths.map(|p| p.data.as_path()),
            cache: paths.map(|p| p.cache.as_path()),
            logs: paths.map(|p| p.logs.as_path()),
            config_dirs: paths
                .map(|p| p.config_search.as_slice())
                .unwrap_or_default(),
            home: home().as_deref(),
        })
    })
}

/// Every directory the rules are built from.
///
/// Named fields because the list grows, and a directory left out of it is not
/// a build error, it is an open door. The cache was left out once, and the
/// model catalog that lives in it picks the base URL for every provider
/// request.
#[derive(Default)]
pub struct Layout<'a> {
    pub state: Option<&'a Path>,
    pub data: Option<&'a Path>,
    /// Closed like the state dir. What Maki keeps here is answers it trusts
    /// on the next start without asking anyone again, and the model catalog
    /// in it carries the `base_url` of every provider that comes from it,
    /// with no signature to check. In the legacy `~/.maki` layout this
    /// directory is the state dir, so closing it also means one answer
    /// instead of two that depend on the layout.
    pub cache: Option<&'a Path>,
    /// Closed for the same reason, plus one of its own: a log holds whatever
    /// the session held, prompts and tool output alike, and `state_logs` puts
    /// it beside the state dir rather than inside it, so the blanket close
    /// does not reach it. `maki.env.logs_dir()` still names the directory for
    /// a plugin that wants to write its own file elsewhere.
    pub logs: Option<&'a Path>,
    pub config_dirs: &'a [PathBuf],
    /// Where `~/.maki` would be. Separate from `config_dirs` because that
    /// list only holds directories that exist, and the point of the rule is
    /// to cover `~/.maki` before anyone creates it.
    pub home: Option<&'a Path>,
}

/// A rule set for one layout.
///
/// The free functions above ask the process-wide one, which is what every
/// caller in Maki wants. A test builds its own instead, because the real
/// layout depends on whether the machine running the test happens to have a
/// `~/.maki`, and an assertion that changes with the developer's home
/// directory says nothing about the guard. Building one touches no
/// filesystem beyond resolving the paths it is handed.
pub struct Guard {
    rules: Rules,
    /// Where the startup Lua lives in a config dir. Not a protection rule:
    /// these paths carry no refusal, they are the ones the permission layer
    /// wants to recognise. A `.maki` directory needs no roots here, because
    /// the shape of the path says it wherever the project sits.
    startup_lua_roots: Vec<PathBuf>,
}

impl Guard {
    pub fn for_layout(layout: &Layout<'_>) -> Self {
        Self {
            rules: protection_rules(layout),
            startup_lua_roots: named_config_dirs(layout.config_dirs, layout.home)
                .iter()
                .flat_map(|dir| {
                    STARTUP_LUA
                        .iter()
                        .map(|name| canonical_key(&dir.join(name)))
                })
                .collect(),
        }
    }

    pub fn protected(&self, path: &Path) -> Option<Protection> {
        stricter(
            self.protected_key(&canonical_key(path)),
            shape_rule(&spelled_key(path)),
        )
    }

    pub fn protected_key(&self, key: &Path) -> Option<Protection> {
        match_rule(&self.rules, key)
    }

    pub fn is_unreachable(&self, path: &Path) -> bool {
        self.protected(path) == Some(Protection::NoAccess)
    }

    pub fn contains_protected(&self, path: &Path) -> bool {
        protects_anything_under(&self.rules, &canonical_key(path), &spelled_key(path))
    }

    /// Both spellings are asked, for the same reason `protected` asks both: a
    /// `.maki` can be a symlink and a file that does not exist yet has no
    /// resolved form to speak of.
    pub fn is_startup_lua(&self, path: &Path) -> bool {
        let key = canonical_key(path);
        let spelled = spelled_key(path);
        under_maki_dir_names(&key, &STARTUP_LUA)
            || under_maki_dir_names(&spelled, &STARTUP_LUA)
            || self
                .startup_lua_roots
                .iter()
                .any(|root| under(&key, root) || under(&spelled, root))
    }
}

/// An open rule does not count. Those name the subtrees Maki's own plugins
/// write in, so their contents belong to the caller anyway.
///
/// A `.maki` directory counts even though no rule is rooted at it, because
/// removing one removes the `permissions.toml` and the `plugin.toml` inside
/// without ever naming them. Removing the project directory around it is a
/// different question and stays allowed: refusing that would mean Maki cannot
/// delete an ordinary folder, and the deletion hands nothing back, since
/// writing those files again is still refused.
fn protects_anything_under(rules: &Rules, key: &Path, spelled: &Path) -> bool {
    let names_maki_dir = |path: &Path| {
        path.file_name()
            .is_some_and(|name| same_name(name, OsStr::new(MAKI_DIR)))
    };
    names_maki_dir(key)
        || names_maki_dir(spelled)
        || rules
            .ordered
            .iter()
            .any(|(root, protection)| protection.is_some() && under(root, key))
}

/// The strictest answer wins. A closed path stays closed, and anything the
/// shape rule names is at least read-only, whatever the layout says about the
/// place it sits in.
fn match_rule(rules: &Rules, key: &Path) -> Option<Protection> {
    let rooted = rules
        .ordered
        .iter()
        .find(|(root, _)| under(key, root))
        .map_or(rules.unmatched, |(_, protection)| *protection);
    stricter(rooted, shape_rule(key))
}

fn stricter(a: Option<Protection>, b: Option<Protection>) -> Option<Protection> {
    match (a, b) {
        (Some(Protection::NoAccess), _) | (_, Some(Protection::NoAccess)) => {
            Some(Protection::NoAccess)
        }
        (Some(protection), _) | (_, Some(protection)) => Some(protection),
        (None, None) => None,
    }
}

fn shape_rule(path: &Path) -> Option<Protection> {
    under_maki_dir_names(path, &MAKI_DIR_READ_ONLY).then_some(Protection::NoWrite)
}

/// The path as it was written, made absolute and tidied up, but not resolved.
///
/// The rooted rules want the resolved path, because what they name is a place
/// on disk, and a symlink or a `..` must not spell a way out of one. The shape
/// rule wants this one, because what it names is a name: a repository can ship
/// its `.maki` as a link to somewhere else, and the Lua loader follows the
/// link and runs whatever is behind it. A file that nobody has created yet is
/// the same story, since `realpath` cannot correct a tail that is not there.
///
/// Asking both and keeping the stricter answer costs one lexical pass and
/// covers both spellings.
fn spelled_key(path: &Path) -> PathBuf {
    normalize_path(&expand_tilde(path))
}

/// Whether two path components name the same file.
///
/// `fs::canonicalize` hands back the spelling that is on disk, and on APFS and
/// NTFS that is whatever spelling the file was created with, while the
/// filesystem still finds the file under every other one. So a `.MAKI` the
/// agent makes today is the `.maki` the loader reads on the next start, and a
/// rule that knows only one spelling protects neither. Linux keeps the two
/// apart, and folding case there would put two genuinely different files under
/// one rule, so the choice is made per target rather than per path.
///
/// A macOS volume can be case sensitive and a Linux one can fold case, so the
/// target is a guess about the filesystem. It is the safe guess either way:
/// the platforms that fold case by default get the stricter comparison, and
/// the platform that does not keeps its rules off files that only look alike.
///
/// ASCII is enough. Every name a rule spells is ASCII, and the components a
/// caller could respell with anything else already exist on disk, so
/// canonicalizing has replaced them with the spelling the rule root was built
/// from.
#[cfg(any(windows, target_os = "macos"))]
fn same_name(a: &OsStr, b: &OsStr) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn same_name(a: &OsStr, b: &OsStr) -> bool {
    a == b
}

/// `Path::starts_with` with `same_name` doing the comparing, so a rule covers
/// every spelling the filesystem answers to.
fn under(key: &Path, root: &Path) -> bool {
    let mut key_parts = key.components();
    root.components().all(|root_part| {
        key_parts
            .next()
            .is_some_and(|key_part| same_name(key_part.as_os_str(), root_part.as_os_str()))
    })
}

/// Whether `key` names one of `entries` directly inside a `.maki` directory,
/// or sits inside such an entry.
///
/// The walk looks for a `.maki` component directly followed by one of those
/// names. Returning as soon as it finds the pair is what gives prefix
/// matching for free: `.maki/lua` matches, and so does every file below it,
/// because the pair is still there further up the path. The rooted rules
/// cannot say this, since a project `.maki` has no fixed place to be rooted
/// at, and Maki opens whatever folder the user points it at.
fn under_maki_dir_names(key: &Path, entries: &[&str]) -> bool {
    let mut in_maki_dir = false;
    for component in key.components() {
        let Component::Normal(name) = component else {
            in_maki_dir = false;
            continue;
        };
        if in_maki_dir
            && entries
                .iter()
                .any(|entry| same_name(name, OsStr::new(entry)))
        {
            return true;
        }
        in_maki_dir = same_name(name, OsStr::new(MAKI_DIR));
    }
    false
}

fn rule(path: &Path, protection: Option<Protection>) -> ProtectionRule {
    (canonical_key(path), protection)
}

/// The rules, narrowest first, because the first one that matches decides.
///
/// The state dir is closed and the open subtrees are named, rather than the
/// other way around. A list of forbidden names only covers the files somebody
/// remembered, so the next thing Maki learns to store would be readable until
/// someone noticed. This way it is covered on the day it is written.
///
/// Packages are read-only instead of closed, because a plugin `require`s Lua
/// modules out of them and reading a package is how anyone reviews what it
/// does. Nothing rewrites a checkout in place anyway, `maki-pack` fetches a new
/// revision beside the old one.
///
/// `CONFIG_READ_ONLY` gets the same treatment for the same reason. Those are
/// the files that say what Maki runs and what it allows, so a write to one of
/// them is not a config edit, it is next session's rule set. Nothing
/// legitimate loses anything: Maki writes `permissions.toml` itself through
/// `append_permission_rule`, which goes straight to `atomic_write` and never
/// through `maki.fs`.
/// One directory sometimes serves two roles: state and user config. That is
/// the legacy `~/.maki` layout, and it is also a stock Windows install, where
/// `etcetera` answers `%APPDATA%\maki` for config and data alike and has no
/// state dir to offer. Both shapes are handled the same way, by comparing the
/// resolved paths, because the user of a shared directory has the same problem
/// either way and no flag can tell the two apart reliably.
///
/// There the blanket close would swallow the config dir, and every skill,
/// command and `config.toml` under it would go unreadable with nothing saying
/// why. The answer is not to open the whole directory back up: it is to keep
/// closing by default and name the config-shaped content that has to stay
/// reachable, `CONFIG_CONTENT` plus `CONFIG_READ_ONLY`.
///
/// That inversion is on purpose. What must stay readable can be listed,
/// because something in Maki reads each entry, and a missing entry breaks a
/// visible feature that someone reports. What must stay closed cannot be
/// listed, because the next state file Maki learns to write is not in any list
/// yet, and a missing entry there leaks in silence. Loud over silent.
///
/// `~/.maki` is ruled in whether or not it exists today, and that is the one
/// rule here that does not come from the resolved layout. Creating that
/// directory is what picks the layout: the next start puts config, data,
/// state and cache in it, so an `init.lua` dropped there becomes global
/// config, and the real state dir stops being the state dir and loses its own
/// rule with it. Waiting for the directory to exist would mean the guard
/// arrives one `mkdir` too late, which is exactly how the hole worked.
///
/// The cache dir and the log dir are closed alongside the state dir, and the
/// fields on `Layout` say what each one holds that earns it.
///
/// When the state dir is unknown, every path is refused. Maki cannot name the
/// files it has to keep, and a rule set that quietly leaves them out reads
/// exactly like a machine with nothing to hide, so it would hand out the
/// credentials on the first ask. Refusing is loud, and the user gets a warning
/// saying the guard is degraded rather than a silent hole.
fn protection_rules(layout: &Layout<'_>) -> Rules {
    let Layout {
        state,
        data,
        cache,
        logs,
        config_dirs,
        home,
    } = *layout;
    let state_key = state.map(canonical_key);
    let legacy = home.map(|h| h.join(MAKI_DIR));
    let named_dirs = named_config_dirs(config_dirs, home);
    let read_only = named_dirs.iter().flat_map(|dir| {
        CONFIG_READ_ONLY
            .iter()
            .map(move |name| rule(&dir.join(name), Some(Protection::NoWrite)))
    });
    let credentials = state
        .into_iter()
        .map(|dir| rule(&credentials_dir(dir), Some(Protection::NoAccess)));
    let open = state.into_iter().flat_map(|dir| {
        OPEN_STATE_SUBTREES
            .iter()
            .map(move |name| rule(&dir.join(name), None))
    });
    let shared_config = config_dirs
        .iter()
        .filter(|dir| state_key.as_deref() == Some(canonical_key(dir).as_path()))
        .flat_map(|dir| {
            CONFIG_CONTENT
                .iter()
                .chain(&STARTUP_LUA)
                .map(move |name| rule(&dir.join(name), None))
        });
    let packages = data
        .into_iter()
        .map(|dir| rule(&dir.join(SITE_DIR), Some(Protection::NoWrite)));
    let closed = state
        .into_iter()
        .chain(legacy.as_deref())
        .chain(cache)
        .chain(logs)
        .map(|dir| rule(dir, Some(Protection::NoAccess)));
    Rules {
        ordered: read_only
            .chain(credentials)
            .chain(open)
            .chain(shared_config)
            .chain(packages)
            .chain(closed)
            .collect(),
        unmatched: state.is_none().then_some(Protection::NoAccess),
    }
}

/// Every directory a config file of Maki's could be read from, which is the
/// search list plus `~/.maki` whether or not anyone has created it yet.
fn named_config_dirs(config_dirs: &[PathBuf], home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = config_dirs.to_vec();
    if let Some(dir) = home.map(|h| h.join(MAKI_DIR)).filter(|d| !dirs.contains(d)) {
        dirs.push(dir);
    }
    dirs
}

/// Asks the code that writes credentials where they go, so moving them moves
/// the rule with them instead of leaving it pointing at an empty directory.
fn credentials_dir(state: &Path) -> PathBuf {
    let any_provider = crate::auth::auth_path(&StateDir::from_path(state.to_path_buf()), APP_NAME);
    any_provider
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| state.to_path_buf())
}

/// Pure core of `config_search_dirs`: no env reads, no process-home fallback,
/// so tests can hand it tempdirs.
pub fn config_search_dirs_from(home: Option<&Path>, xdg_config: Option<&Path>) -> Vec<PathBuf> {
    let legacy = home.map(|h| h.join(MAKI_DIR)).filter(|d| d.is_dir());
    let xdg = xdg_config
        .map(Path::to_path_buf)
        .filter(|d| Some(d) != legacy.as_ref());
    [legacy, xdg].into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const KEYED_FILE: &str = "f.rs";
    const SUBDIR: &str = "sub";
    const CONFIG_FILE: &str = "config.toml";
    const NOTE_FILE: &str = "note.md";
    const SKILL_FILE: &str = "skills/reviewing/SKILL.md";
    const PROVIDER: &str = "anthropic";
    const PROVIDER_SCRIPT: &str = "providers/mycorp";
    const PROVIDERS_SIBLING: &str = "providers-old";
    const LUA_MODULE: &str = "lua/browser.lua";
    const SIBLING_SUFFIX: &str = "-old";
    #[cfg(unix)]
    const LEXICAL_ESCAPE: &str = "a lexical spelling must not decide the rule";
    const ABSENT_LEGACY: &str = "the point of the rule is a directory that is not there yet";
    const FROZEN_LAYOUT: &str = "an env var set after startup must not move Maki's own directories";
    const FAIL_CLOSED: &str = "a guard that cannot be built must refuse, not allow";
    const COMMAND_FILE: &str = "commands/review.md";
    const SKILLS_SIBLING: &str = "skills-old";
    const XDG_CONFIG_REL: &str = ".config";
    const PROJECT_ENTRY_POINT: &str = ".maki/init.lua";
    const PROJECT_LUA_MODULE: &str = ".maki/lua/helper.lua";
    const PROJECT_PLUGIN_MANIFEST: &str = ".maki/plugin.toml";
    const PROJECT_PERMISSIONS: &str = ".maki/permissions.toml";
    const PROJECT_MCP_CONFIG: &str = ".maki/mcp.toml";
    const PROJECT_ENV_FILE: &str = ".maki/.env";
    const PROJECT_PLAN: &str = ".maki/plans/123.md";
    const MAKI_DIR_SIBLING: &str = ".maki-old/init.lua";
    const RESPELLED_MAKI_DIR: &str = ".MAKI/permissions.toml";
    const RESPELLED_PERMISSIONS: &str = ".maki/Permissions.Toml";
    const STATE_ROLE: &str = "state";
    const CACHE_ROLE: &str = "cache";
    const LOGS_ROLE: &str = "logs";
    const CASE_SPELLING: &str =
        "where the filesystem folds case, another spelling is the same file";
    /// What the target says about the filesystem underneath it, which is the
    /// same guess `same_name` makes.
    const CASE_FOLDING_FS: bool = cfg!(any(windows, target_os = "macos"));
    const WINDOWS_APPDATA_REL: &str = "AppData/Roaming";

    /// What a directory that is both Maki's state and the user's config owes
    /// each side. The state half is closed unless a rule opens it, the config
    /// half stays reachable, what decides Maki's behaviour is readable but not
    /// writable, and the startup Lua is the user's to edit here as anywhere
    /// else.
    const SHARED_DIRECTORY_EXPECTATIONS: [(&str, Option<Protection>); 9] = [
        (NOTE_FILE, Some(Protection::NoAccess)),
        (SKILLS_SIBLING, Some(Protection::NoAccess)),
        (PERMISSIONS_FILE, Some(Protection::NoWrite)),
        (SITE_DIR, Some(Protection::NoWrite)),
        (CONFIG_ENTRY_POINT, None),
        (LUA_MODULE, None),
        (CONFIG_FILE, None),
        (SKILL_FILE, None),
        (COMMAND_FILE, None),
    ];

    /// The two ways one directory ends up serving two roles.
    #[derive(Clone, Copy)]
    enum Collapsed {
        /// `~/.maki` exists, so every role collapses onto it.
        LegacyHome,
        /// `etcetera` answers `%APPDATA%\maki` for config and data, and has no
        /// state dir to offer, so Maki puts its state there too.
        StockWindows,
    }

    /// The directories most tests have a question about, so each one names
    /// only those and leaves the rest of the layout empty.
    fn rules_from(
        state: Option<&Path>,
        data: Option<&Path>,
        config_dirs: &[PathBuf],
        home: Option<&Path>,
    ) -> Rules {
        protection_rules(&Layout {
            state,
            data,
            config_dirs,
            home,
            ..Layout::default()
        })
    }

    fn protection_of(rules: &Rules, path: &Path) -> Option<Protection> {
        match_rule(rules, &canonical_key(path))
    }

    fn protects_under(rules: &Rules, path: &Path) -> bool {
        protects_anything_under(rules, &canonical_key(path), &spelled_key(path))
    }

    /// Builds the rules the way `rules()` does, from a home directory shaped
    /// like the platform in question, so the test asks the same question the
    /// process does rather than a hand-written stand-in.
    fn collapsed_rules(home: &Path, shape: Collapsed) -> (Rules, PathBuf) {
        let (shared, xdg_config) = match shape {
            Collapsed::LegacyHome => (
                home.join(MAKI_DIR),
                home.join(XDG_CONFIG_REL).join(APP_NAME),
            ),
            Collapsed::StockWindows => {
                let appdata = home.join(WINDOWS_APPDATA_REL).join(APP_NAME);
                (appdata.clone(), appdata)
            }
        };
        fs::create_dir_all(&shared).unwrap();
        let config_dirs = config_search_dirs_from(Some(home), Some(&xdg_config));
        let rules = rules_from(Some(&shared), Some(&shared), &config_dirs, Some(home));
        (rules, shared)
    }

    /// Asks the code that stores credentials where it puts them, so moving
    /// them cannot move them out from under the rule without this failing.
    fn credentials_in(state: &Path) -> PathBuf {
        crate::auth::auth_path(&StateDir::from_path(state.to_path_buf()), PROVIDER)
    }

    #[test]
    fn stored_credentials_are_unreachable() {
        let state = tempfile::TempDir::new().unwrap();
        let rules = rules_from(Some(state.path()), None, &[], None);

        assert_eq!(
            protection_of(&rules, &credentials_in(state.path())),
            Some(Protection::NoAccess)
        );
    }

    /// Nobody had to name this file for it to be covered, which is the whole
    /// point of closing the directory instead of listing what is in it.
    #[test]
    fn a_state_file_nobody_listed_is_closed() {
        let state = tempfile::TempDir::new().unwrap();
        let rules = rules_from(Some(state.path()), None, &[], None);

        assert_eq!(
            protection_of(&rules, &state.path().join(NOTE_FILE)),
            Some(Protection::NoAccess)
        );
    }

    /// A guard that cannot find the state dir used to answer "no rule applies"
    /// for the credentials, which reads the same as "help yourself".
    #[test_case(credentials_in; "credentials")]
    #[test_case(|state| state.join(NOTE_FILE); "a_state_file")]
    #[test_case(|_| PathBuf::from("/etc").join(NOTE_FILE); "any_other_path")]
    fn an_unknown_state_dir_refuses(spell: fn(&Path) -> PathBuf) {
        let would_be_state = tempfile::TempDir::new().unwrap();
        let rules = rules_from(None, None, &[], None);

        assert_eq!(
            protection_of(&rules, &spell(would_be_state.path())),
            Some(Protection::NoAccess),
            "{FAIL_CLOSED}"
        );
    }

    #[test]
    fn every_open_subtree_stays_open() {
        let state = tempfile::TempDir::new().unwrap();
        let rules = rules_from(Some(state.path()), None, &[], None);

        for name in OPEN_STATE_SUBTREES {
            let open = state.path().join(name);
            assert_eq!(protection_of(&rules, &open.join(NOTE_FILE)), None, "{name}");
            assert_eq!(
                protection_of(
                    &rules,
                    &open.with_file_name(name.to_owned() + SIBLING_SUFFIX)
                ),
                Some(Protection::NoAccess),
                "a name that only starts the same is a different directory"
            );
        }
    }

    /// Neither of these sits under the state dir in the XDG layout, so the
    /// blanket close does not reach them and they need rules of their own.
    /// The cache holds the model catalog, which carries the `base_url` of
    /// every provider built from it, and the logs hold whatever the session
    /// held. In the legacy `~/.maki` layout both collapse onto the state dir
    /// and were already closed, which is how one file ended up with two
    /// different answers depending on the layout.
    #[test_case(|cache, _| cache.to_path_buf(); "the_cache_dir")]
    #[test_case(|cache, _| cache.join(NOTE_FILE); "a_cached_file_nobody_listed")]
    #[test_case(|_, logs| logs.join(NOTE_FILE); "a_log_file")]
    fn the_cache_and_the_logs_are_closed(spell: fn(&Path, &Path) -> PathBuf) {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join(STATE_ROLE);
        let cache = root.path().join(CACHE_ROLE);
        let logs = root.path().join(LOGS_ROLE);
        let rules = protection_rules(&Layout {
            state: Some(&state),
            cache: Some(&cache),
            logs: Some(&logs),
            ..Layout::default()
        });

        assert_eq!(
            protection_of(&rules, &spell(&cache, &logs)),
            Some(Protection::NoAccess)
        );
    }

    /// Plan mode allocates a path and then has the agent write the plan there
    /// with the `write` tool, so closing `plans` does not fail loudly, it
    /// leaves plan mode unable to finish: the write is refused, and the
    /// trigger that waits for it never fires. The path comes from the code
    /// that allocates it, so moving plans elsewhere cannot move them out from
    /// under this rule in silence.
    #[test]
    fn the_agent_can_write_the_plan_it_was_asked_for() {
        let state = tempfile::TempDir::new().unwrap();
        let rules = rules_from(Some(state.path()), None, &[], None);
        let dir = StateDir::from_path(state.path().to_path_buf());
        let plan = crate::plans::new_plan_path(&dir).unwrap();

        assert_eq!(protection_of(&rules, &plan), None);
    }

    #[test]
    fn packages_are_read_only() {
        let data = tempfile::TempDir::new().unwrap();
        let rules = rules_from(None, Some(data.path()), &[], None);

        assert_eq!(
            protection_of(&rules, &data.path().join(SITE_DIR).join(NOTE_FILE)),
            Some(Protection::NoWrite)
        );
    }

    /// A project's own `.maki` decides what Maki may do there without asking.
    /// The user made that call by trusting the folder, and the trust record
    /// keeps names rather than content, so an agent rewriting these files
    /// would be answering for them.
    ///
    /// The startup Lua is the exception: writing a plugin is a thing users
    /// ask for, so it is an ordinary path here and the permission layer asks
    /// about the write instead.
    #[test_case(PROJECT_ENTRY_POINT, None; "the_entry_point")]
    #[test_case(PROJECT_LUA_MODULE, None; "a_required_lua_module")]
    #[test_case(PROJECT_PLUGIN_MANIFEST, Some(Protection::NoWrite); "plugin_manifest")]
    #[test_case(PROJECT_PERMISSIONS, Some(Protection::NoWrite); "permissions")]
    #[test_case(PROJECT_MCP_CONFIG, Some(Protection::NoWrite); "mcp_servers")]
    #[test_case(PROJECT_ENV_FILE, Some(Protection::NoWrite); "env_file")]
    #[test_case(PROJECT_PLAN, None; "makis_own_project_scratch_space")]
    #[test_case(CONFIG_ENTRY_POINT, None; "the_same_name_outside_a_maki_dir")]
    #[test_case(MAKI_DIR_SIBLING, None; "a_dir_that_only_starts_the_same")]
    fn a_maki_dir_is_read_only_wherever_it_sits(rel: &str, expected: Option<Protection>) {
        let project = tempfile::TempDir::new().unwrap();
        let state = tempfile::TempDir::new().unwrap();
        let rules = rules_from(Some(state.path()), None, &[], None);

        assert_eq!(protection_of(&rules, &project.path().join(rel)), expected);
    }

    /// A directory the agent creates as `.MAKI` canonicalizes back to
    /// `.MAKI`, because APFS and NTFS keep the spelling they were handed, and
    /// the next start still opens it as `.maki`. Whether a real file behaves
    /// that way depends on the volume the test runs on, so the comparison is
    /// asked directly instead of making a file and hoping.
    #[test_case(RESPELLED_MAKI_DIR; "the_maki_dir_itself")]
    #[test_case(RESPELLED_PERMISSIONS; "an_entry_inside_it")]
    fn a_respelled_maki_dir_matches_where_case_folds(rel: &str) {
        assert_eq!(
            under_maki_dir_names(Path::new(rel), &MAKI_DIR_READ_ONLY),
            CASE_FOLDING_FS,
            "{CASE_SPELLING}"
        );
    }

    #[test_case(STATE_ROLE, true; "the_same_spelling")]
    #[test_case("STATE", CASE_FOLDING_FS; "another_spelling")]
    #[test_case("state-old", false; "a_name_that_only_starts_the_same")]
    fn a_rooted_rule_covers_what_the_filesystem_answers_to(dir: &str, expected: bool) {
        let root = Path::new("/x").join(STATE_ROLE);
        let key = Path::new("/x").join(dir).join(NOTE_FILE);

        assert_eq!(under(&key, &root), expected, "{CASE_SPELLING}");
    }

    /// A repository can ship its own `.maki` as a link to somewhere else, and
    /// the Lua loader follows the link and runs what is behind it. Resolving
    /// the path first throws away the very name this rule is about, so the
    /// spelling gets asked as well and the stricter answer wins.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_maki_dir_is_still_read_only() {
        let project = tempfile::TempDir::new().unwrap();
        let elsewhere = tempfile::TempDir::new().unwrap();
        let state = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), project.path().join(MAKI_DIR)).unwrap();
        let guard = Guard::for_layout(&Layout {
            state: Some(state.path()),
            ..Layout::default()
        });
        let permissions = project.path().join(PROJECT_PERMISSIONS);

        assert_eq!(guard.protected(&permissions), Some(Protection::NoWrite));
        assert_eq!(
            match_rule(&guard.rules, &canonical_key(&permissions)),
            None,
            "the link has to be one that resolving the path walks straight past"
        );
        assert!(
            guard.contains_protected(&project.path().join(MAKI_DIR)),
            "removing the link takes the permissions file with it"
        );
    }

    /// Removing the directory takes the same files without naming one of
    /// them, and the project around it has to stay removable.
    #[test_case(|project| project.join(MAKI_DIR), true; "the_maki_dir")]
    #[test_case(|project| project.to_path_buf(), false; "the_project_around_it")]
    fn removing_a_project_maki_dir(spell: fn(&Path) -> PathBuf, expected: bool) {
        let project = tempfile::TempDir::new().unwrap();
        let state = tempfile::TempDir::new().unwrap();
        let rules = rules_from(Some(state.path()), None, &[], None);

        assert_eq!(protects_under(&rules, &spell(project.path())), expected);
    }

    /// A write to any of these is not a config edit, it is what Maki runs and
    /// allows the next time it starts. `permissions.toml` is the sharp one: a
    /// `default = "allow"` in there ungates `bash`, and `bash` walks around
    /// every other rule here.
    #[test_case(CONFIG_ENTRY_POINT, None; "the_entry_point_is_the_users_to_write")]
    #[test_case(LUA_MODULE, None; "so_is_a_required_lua_module")]
    #[test_case(PLUGIN_MANIFEST, Some(Protection::NoWrite); "plugin_manifest")]
    #[test_case(PERMISSIONS_FILE, Some(Protection::NoWrite); "permissions")]
    #[test_case(PROVIDERS_DIR, Some(Protection::NoWrite); "providers_dir")]
    #[test_case(PROVIDER_SCRIPT, Some(Protection::NoWrite); "a_provider_script")]
    #[test_case(ENV_FILE, Some(Protection::NoWrite); "env_file")]
    #[test_case(MCP_CONFIG, Some(Protection::NoWrite); "mcp_servers")]
    #[test_case(PROVIDERS_CONFIG, Some(Protection::NoWrite); "provider_endpoints")]
    #[test_case(PROVIDERS_SIBLING, None; "a_name_that_only_starts_the_same")]
    #[test_case(CONFIG_FILE, None; "ordinary_config_file")]
    #[test_case(SUBDIR, None; "plugin_dir")]
    fn what_decides_makis_behaviour_is_read_only(rel: &str, expected: Option<Protection>) {
        let state = tempfile::TempDir::new().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let rules = rules_from(
            Some(state.path()),
            None,
            &[config.path().to_path_buf()],
            None,
        );

        assert_eq!(protection_of(&rules, &config.path().join(rel)), expected);
    }

    /// Nothing here refuses anything. The permission layer asks this before
    /// it treats a write as ordinary project content, so a plugin the user
    /// asked for gets a prompt rather than a silent yes.
    #[test_case(|config, _| config.join(CONFIG_ENTRY_POINT), true; "the_entry_point_of_a_config_dir")]
    #[test_case(|config, _| config.join(LUA_MODULE), true; "a_module_of_a_config_dir")]
    #[test_case(|_, project| project.join(PROJECT_ENTRY_POINT), true; "the_entry_point_of_a_maki_dir")]
    #[test_case(|_, project| project.join(PROJECT_LUA_MODULE), true; "a_module_of_a_maki_dir")]
    #[test_case(|config, _| config.join(CONFIG_FILE), false; "an_ordinary_config_file")]
    #[test_case(|_, project| project.join(LUA_MODULE), false; "the_same_names_in_a_plain_project")]
    fn startup_lua_is_recognised_wherever_it_runs_from(
        spell: fn(&Path, &Path) -> PathBuf,
        expected: bool,
    ) {
        let config = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let config_dirs = [config.path().to_path_buf()];
        let guard = Guard::for_layout(&Layout {
            config_dirs: &config_dirs,
            ..Layout::default()
        });

        assert_eq!(
            guard.is_startup_lua(&spell(config.path(), project.path())),
            expected
        );
    }

    /// Creating `~/.maki` is what picks the layout for the next start, so the
    /// rule has to be in place before the directory is. Here the state lives
    /// somewhere else entirely, which is the ordinary setup where the hole
    /// used to open: nothing named `~/.maki`, so the agent could write an
    /// `init.lua` into it and own the following session.
    #[test_case(CONFIG_ENTRY_POINT, Some(Protection::NoAccess); "the_entry_point")]
    #[test_case(PERMISSIONS_FILE, Some(Protection::NoWrite); "permissions")]
    #[test_case(NOTE_FILE, Some(Protection::NoAccess); "anything_else_in_it")]
    fn a_legacy_home_is_ruled_in_before_it_exists(rel: &str, expected: Option<Protection>) {
        let home = tempfile::TempDir::new().unwrap();
        let state = tempfile::TempDir::new().unwrap();
        let xdg = home.path().join(XDG_CONFIG_REL).join(APP_NAME);
        let config_dirs = config_search_dirs_from(Some(home.path()), Some(&xdg));
        let rules = rules_from(Some(state.path()), None, &config_dirs, Some(home.path()));
        let legacy = home.path().join(MAKI_DIR);

        assert!(!legacy.exists(), "{ABSENT_LEGACY}");
        assert_eq!(
            protection_of(&rules, &legacy),
            Some(Protection::NoAccess),
            "the directory itself decides the layout, so it cannot be created"
        );
        assert_eq!(protection_of(&rules, &legacy.join(rel)), expected);
    }

    /// One directory in two roles, which happens on the legacy `~/.maki` and
    /// on a stock Windows install alike. The state stays closed by default and
    /// the config-shaped content is named, so nobody loses their skills and
    /// nobody gains a look at the credentials.
    #[test_case(Collapsed::LegacyHome; "legacy_home")]
    #[test_case(Collapsed::StockWindows; "stock_windows")]
    fn a_shared_directory_closes_state_and_keeps_config(shape: Collapsed) {
        let home = tempfile::TempDir::new().unwrap();
        let (rules, shared) = collapsed_rules(home.path(), shape);

        for (rel, expected) in SHARED_DIRECTORY_EXPECTATIONS {
            assert_eq!(protection_of(&rules, &shared.join(rel)), expected, "{rel}");
        }
        assert_eq!(
            protection_of(&rules, &credentials_in(&shared)),
            Some(Protection::NoAccess),
            "credentials stay unreachable in the shared directory"
        );
    }

    /// A recursive removal never names the files it destroys, so asking about
    /// the path alone answers the wrong question: the parent of the state dir
    /// is an ordinary path, and deleting it takes the credentials.
    #[test_case(|state, _, _| state.parent().unwrap().to_path_buf(), true; "above_the_state_dir")]
    #[test_case(|state, _, _| state.to_path_buf(), true; "the_state_dir")]
    #[test_case(|_, _, config| config.to_path_buf(), true; "a_config_dir_holding_permissions")]
    #[test_case(|_, data, _| data.to_path_buf(), true; "above_the_packages")]
    #[test_case(|state, _, _| state.join(OPEN_STATE_SUBTREES[0]), false; "an_open_subtree")]
    #[test_case(|_, _, config| config.join(SUBDIR), false; "an_ordinary_config_subdir")]
    fn a_recursive_removal_sees_what_is_under_it(
        spell: fn(&Path, &Path, &Path) -> PathBuf,
        expected: bool,
    ) {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join("state");
        let data = root.path().join("data");
        let config = root.path().join("config");
        let rules = rules_from(
            Some(&state),
            Some(&data),
            std::slice::from_ref(&config),
            None,
        );

        assert_eq!(
            protects_under(&rules, &spell(&state, &data, &config)),
            expected
        );
    }

    /// The layout nearly everyone is on must not pay for the legacy one: with
    /// separate dirs the state dir is closed by default, whatever is added to
    /// it next.
    #[test_case(|state, _, _| state.join(NOTE_FILE), Some(Protection::NoAccess); "a_state_file_is_closed")]
    #[test_case(|state, _, _| credentials_in(state), Some(Protection::NoAccess); "credentials_are_closed")]
    #[test_case(|state, _, _| state.join(OPEN_STATE_SUBTREES[0]).join(NOTE_FILE), None; "an_open_subtree_stays_open")]
    #[test_case(|_, data, _| data.join(SITE_DIR).join(NOTE_FILE), Some(Protection::NoWrite); "a_package_is_read_only")]
    #[test_case(|_, _, config| config.join(PERMISSIONS_FILE), Some(Protection::NoWrite); "the_permissions_file_is_read_only")]
    #[test_case(|_, _, config| config.join(CONFIG_ENTRY_POINT), None; "the_entry_point_is_writable")]
    #[test_case(|_, _, config| config.join(CONFIG_FILE), None; "a_config_file_is_open")]
    #[test_case(|_, _, config| config.join(SKILL_FILE), None; "a_user_skill_is_open")]
    fn separate_dirs_keep_the_state_dir_closed(
        spell: fn(&Path, &Path, &Path) -> PathBuf,
        expected: Option<Protection>,
    ) {
        let state = tempfile::TempDir::new().unwrap();
        let data = tempfile::TempDir::new().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let rules = rules_from(
            Some(state.path()),
            Some(data.path()),
            &[config.path().to_path_buf()],
            None,
        );

        assert_eq!(
            protection_of(&rules, &spell(state.path(), data.path(), config.path())),
            expected
        );
    }

    #[cfg(unix)]
    #[test_case("", |link| link.join(NOTE_FILE); "through_a_symlink")]
    #[test_case(SUBDIR, |link| link.join("..").join(NOTE_FILE); "back_out_of_a_symlink")]
    fn a_spelling_cannot_escape_the_rule(link_target: &str, spell: fn(&Path) -> PathBuf) {
        let state = tempfile::TempDir::new().unwrap();
        let elsewhere = tempfile::TempDir::new().unwrap();
        let target = state.path().join(link_target);
        fs::create_dir_all(&target).unwrap();
        let link = elsewhere.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let rules = rules_from(Some(state.path()), None, &[], None);

        let spelled = spell(&link);
        assert_eq!(
            protection_of(&rules, &spelled),
            Some(Protection::NoAccess),
            "{LEXICAL_ESCAPE}"
        );
        assert_eq!(
            match_rule(&rules, &normalize_path(&spelled)),
            None,
            "the spelling has to be one that lexical matching misses"
        );
    }

    #[test_case(|_rel, abs| abs.join(KEYED_FILE); "absolute")]
    #[test_case(|rel, _abs| rel.join(KEYED_FILE); "relative")]
    #[test_case(|rel, _abs| rel.join(SUBDIR).join("..").join(KEYED_FILE); "parent_component")]
    fn every_spelling_of_one_file_is_one_key(spell: fn(&Path, &Path) -> PathBuf) {
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::TempDir::new_in(&cwd).unwrap();
        let abs = dir.path();
        let rel = PathBuf::from(abs.file_name().unwrap());
        fs::create_dir(abs.join(SUBDIR)).unwrap();

        let expected = canonical_key(&abs.join(KEYED_FILE));
        assert_eq!(
            canonical_key(&spell(&rel, abs)),
            expected,
            "before the file exists"
        );

        fs::write(abs.join(KEYED_FILE), "content").unwrap();
        assert_eq!(
            canonical_key(&spell(&rel, abs)),
            expected,
            "once the file exists"
        );
    }

    #[test]
    fn tilde_spelling_is_one_key() {
        let home = home().expect("no home dir");
        assert_eq!(
            canonical_key(Path::new("~").join(KEYED_FILE).as_path()),
            canonical_key(&home.join(KEYED_FILE))
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_spelling_is_one_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().join(SUBDIR);
        let link = dir.path().join("link");
        fs::create_dir(&real).unwrap();
        fs::write(real.join(KEYED_FILE), "content").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(
            canonical_key(&link.join(KEYED_FILE)),
            canonical_key(&real.join(KEYED_FILE))
        );
    }

    #[test]
    fn normalize_path_resolves_parent() {
        let cwd = std::env::current_dir().unwrap();
        let input = cwd.join("a").join("b").join("..").join("c");
        let expected = cwd.join("a").join("c");
        assert_eq!(normalize_path(&input), expected);
    }

    #[test]
    fn normalize_path_resolves_dot() {
        let cwd = std::env::current_dir().unwrap();
        let input = cwd.join("a").join(".").join("b");
        let expected = cwd.join("a").join("b");
        assert_eq!(normalize_path(&input), expected);
    }

    #[test]
    fn normalize_path_does_not_pop_past_root() {
        // /../etc should produce /etc, not the relative "etc"
        let result = normalize_path(Path::new("/../etc"));
        assert!(result.is_absolute(), "must stay absolute: {result:?}");
        #[cfg(unix)]
        assert_eq!(result, PathBuf::from("/etc"));
    }

    #[test]
    #[cfg(windows)]
    fn strip_extended_prefix_local_drive() {
        let input = Path::new(r"\\?\C:\Users\test\file.txt");
        let result = strip_windows_extended_prefix(input);
        assert_eq!(result, PathBuf::from(r"C:\Users\test\file.txt"));
    }

    #[test]
    #[cfg(windows)]
    fn strip_extended_prefix_unc_share() {
        let input = Path::new(r"\\?\UNC\server\share\dir\file.txt");
        let result = strip_windows_extended_prefix(input);
        assert_eq!(result, PathBuf::from(r"\\server\share\dir\file.txt"));
    }

    #[test]
    #[cfg(windows)]
    fn strip_extended_prefix_no_prefix() {
        let input = Path::new(r"C:\already\normal\path.txt");
        let result = strip_windows_extended_prefix(input);
        assert_eq!(result, PathBuf::from(r"C:\already\normal\path.txt"));
    }

    #[test]
    #[cfg(windows)]
    fn canonicalize_clean_strips_extended_prefix() {
        let tmp = std::env::temp_dir();
        let result = canonicalize_clean(&tmp);
        let s = result.to_str().unwrap();
        assert!(
            !s.starts_with(r"\\?\"),
            "should not have \\\\?\\ prefix: {s}"
        );
    }

    #[test]
    fn search_dirs_returns_legacy_and_xdg() {
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join(MAKI_DIR);
        let xdg = home.path().join(".config").join(APP_NAME);
        fs::create_dir(&legacy).unwrap();

        let dirs = config_search_dirs_from(Some(home.path()), Some(&xdg));
        assert_eq!(dirs, vec![legacy, xdg]);
    }

    #[test]
    fn search_dirs_omits_legacy_when_it_does_not_exist() {
        let home = tempfile::tempdir().unwrap();
        let xdg = home.path().join(".config").join(APP_NAME);

        let dirs = config_search_dirs_from(Some(home.path()), Some(&xdg));
        assert_eq!(dirs, vec![xdg]);
    }

    #[test]
    fn search_dirs_omits_legacy_when_home_none() {
        let xdg = tempfile::tempdir().unwrap();

        let dirs = config_search_dirs_from(None, Some(xdg.path()));
        assert_eq!(dirs, vec![xdg.path().to_path_buf()]);
    }

    #[test]
    fn search_dirs_omits_xdg_when_xdg_none() {
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join(MAKI_DIR);
        fs::create_dir(&legacy).unwrap();

        let dirs = config_search_dirs_from(Some(home.path()), None);
        assert_eq!(dirs, vec![legacy]);
    }

    #[test]
    fn search_dirs_does_not_repeat_the_same_dir() {
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join(MAKI_DIR);
        fs::create_dir(&legacy).unwrap();

        let dirs = config_search_dirs_from(Some(home.path()), Some(&legacy));
        assert_eq!(dirs, vec![legacy]);
    }

    #[test]
    fn search_dirs_neither_depends_on_process_env() {
        let home_a = tempfile::tempdir().unwrap();
        let xdg_a = home_a.path().join(".config").join(APP_NAME);

        let hostile = tempfile::tempdir().unwrap();

        let prev = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: tests run single-threaded within a process nextest invokes once.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", hostile.path()) };

        let dirs = config_search_dirs_from(Some(home_a.path()), Some(&xdg_a));

        // SAFETY: same single-threaded assumption as above.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        assert!(
            !dirs.iter().any(|p| p.starts_with(hostile.path())),
            "combiner read XDG_CONFIG_HOME: {dirs:?}"
        );
    }

    /// Maki sets environment variables on itself while it starts, because
    /// that is what loading an `.env` file does. If the layout followed those
    /// variables, an `.env` could point the config dir at a directory the
    /// agent controls, and everything below would follow it there.
    #[test]
    fn the_layout_stays_put_when_the_environment_moves() {
        let hostile = tempfile::tempdir().unwrap();
        // A ready-made `.maki` in it, because that is what a layout worked
        // out again later would pick up: the legacy dir wins over the XDG one.
        fs::create_dir(hostile.path().join(MAKI_DIR)).unwrap();
        freeze();
        let before = (home(), config_search_dirs());

        let previous = ["HOME", "XDG_CONFIG_HOME"].map(|key| (key, std::env::var_os(key)));
        // SAFETY: nextest runs each test in a process of its own.
        unsafe {
            for (key, _) in previous.iter() {
                std::env::set_var(key, hostile.path());
            }
        }

        let after = (home(), config_search_dirs());

        // SAFETY: same as above, and restoring keeps the process honest for
        // whatever the harness does next.
        unsafe {
            for (key, value) in previous {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }

        assert_eq!(before, after, "{FROZEN_LAYOUT}");
        assert!(
            !after.1.iter().any(|d| d.starts_with(hostile.path())),
            "{FROZEN_LAYOUT}"
        );
    }
}
