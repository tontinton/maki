//! Where Maki's own files live, and which of them the agent may touch.
//!
//! `Guard` answers the second question. It closes Maki's own directories and
//! names what stays open, never the other way around. A list of forbidden
//! names only covers what somebody remembered, while a missing open subtree
//! breaks a visible feature and someone reports it.
//!
//! Which way the blanket leans decides whether a forgotten file is dangerous.
//! A read here can be handed over by a human answer, a write never can. People
//! do want the agent to read a log or quote a session, but Maki believes
//! whatever it finds in these directories on its next start, so the file that
//! lands there tomorrow is safe without anyone naming it today.
//!
//! None of this helps while the `bash` tool is ungated: a shell command reads
//! the same files directly and only an OS sandbox stops that.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use arc_swap::ArcSwap;
use etcetera::base_strategy::BaseStrategy;
use thiserror::Error;

/// Maki's own directory name, in a home dir (`~/.maki`, the legacy layout) and
/// in a project (`<project>/.maki`) alike.
const MAKI_DIR: &str = ".maki";
const APP_NAME: &str = "maki";

/// Where the store of package approvals lives, under the state dir. Named here
/// rather than beside its reader in `maki-lua` so the rule that closes it and
/// the code that writes it point at one file.
pub const APPROVALS_FILE: &str = "pack-approvals.json";
/// Where packages are checked out, under the data dir. Public so the checkout
/// code and the rule covering it cannot drift apart.
pub const SITE_DIR: &str = "site";

/// The config-dir names that hold something only the user may have, shared with
/// the loaders that read them. Only the closed ones get a shared constant: a
/// rename that drifts from an open entry breaks a feature somebody reports,
/// while one that drifts from a closed entry stops protecting a file and
/// nobody notices.
pub const PERMISSIONS_FILE: &str = "permissions.toml";
pub const ENV_FILE: &str = ".env";
pub const PROVIDERS_FILE: &str = "providers.toml";
pub const MCP_FILE: &str = "mcp.toml";
pub const INIT_LUA: &str = "init.lua";

/// Where a plugin may keep its own state, under the state dir. Open, because a
/// plugin has nowhere else to put a file and `maki.fs` is its only way to write
/// one.
pub const PLUGIN_STATE_DIR: &str = "plugins";

/// What lives in the state dir and who may touch it, overlaid on the blanket
/// close the directory itself gets.
///
/// The open entries are what features depend on: memory notes, the generated
/// Lua API reference, the plan written in plan mode, and whatever a plugin
/// keeps for itself. Drop a name from here and its feature quietly stops
/// working.
///
/// The two closed entries repeat what the blanket rule says anyway. The agent
/// is the one reading the refusal, and "this file holds provider credentials"
/// tells it more than "Maki keeps its own state here".
const STATE_ENTRIES: [(&str, Reach); 6] = [
    (
        crate::auth::AUTH_DIR,
        Reach::Closed(Refusal::Credentials, Override::Never),
    ),
    (
        APPROVALS_FILE,
        Reach::Closed(Refusal::ApprovalStore, Override::Never),
    ),
    ("projects", Reach::Open),
    ("docs", Reach::Open),
    ("plans", Reach::Open),
    (PLUGIN_STATE_DIR, Reach::Open),
];

/// What lives in the data dir. Nothing else does today, which is the point:
/// whatever lands there next inherits the blanket close on its own.
const DATA_ENTRIES: [(&str, Reach); 1] = [(
    SITE_DIR,
    Reach::ReadOnly(Refusal::PackageCode, Override::Never),
)];

/// What a `.maki` directory holds, wherever one turns up.
///
/// These four files decide what Maki does with the folder around them. Folder
/// trust records a yes by file name and never by contents, so a file the agent
/// rewrote after the yes is honoured with nobody asked again. The permission
/// layer is no help either, since it allows writes anywhere under the opened
/// folder, which is exactly where a project's `.maki` sits.
///
/// Matched by shape rather than by place, because Maki resolves one project
/// and the agent can reach many: a sibling checkout, a vendored copy, a
/// repository it just cloned into `/tmp`.
///
/// `init.lua` is the one entry an approval can open. People do ask for edits
/// to the Lua a repository runs, and unlike the other three it hands over no
/// secret on its own.
const MAKI_DIR_ENTRIES: [(&str, Reach); 4] = [
    (
        ENV_FILE,
        Reach::Closed(Refusal::Credentials, Override::Never),
    ),
    (
        MCP_FILE,
        Reach::Closed(Refusal::Credentials, Override::Never),
    ),
    (
        PERMISSIONS_FILE,
        Reach::ReadOnly(Refusal::PermissionPolicy, Override::Never),
    ),
    (
        INIT_LUA,
        Reach::ReadOnly(Refusal::StartupCode, Override::ByApproval),
    ),
];

/// Every name that turns up in a config dir, and what the agent may do with it.
/// Applied per name to every config dir, so one file keeps its answer even
/// where `etcetera` collapses config, state and data onto a single directory.
///
/// `Closed` is for plaintext credentials, and a name joins that group the day
/// a credential-shaped field lands in the file it points at. Maki's own loaders
/// read those in Rust without asking the guard, so closing one costs no
/// feature. `Open` is content the user keeps and hands to the agent.
///
/// `permissions.toml` is neither. It is where the user writes down what the
/// agent may do, including which of Maki's own files an approval opened, so the
/// agent reads it and never writes it.
const CONFIG_ENTRIES: [(&str, Reach); 13] = [
    (
        ENV_FILE,
        Reach::Closed(Refusal::Credentials, Override::Never),
    ),
    (
        PROVIDERS_FILE,
        Reach::Closed(Refusal::Credentials, Override::Never),
    ),
    (
        MCP_FILE,
        Reach::Closed(Refusal::Credentials, Override::Never),
    ),
    (
        PERMISSIONS_FILE,
        Reach::ReadOnly(Refusal::PermissionPolicy, Override::Never),
    ),
    ("config.toml", Reach::Open),
    (INIT_LUA, Reach::Open),
    ("lua", Reach::Open),
    ("pack-lock.json", Reach::Open),
    ("providers", Reach::Open),
    ("AGENTS.md", Reach::Open),
    ("skills", Reach::Open),
    ("commands", Reach::Open),
    ("themes", Reach::Open),
];

static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
static STRATEGY: OnceLock<Option<Paths>> = OnceLock::new();
static PROTECTION_RULES: OnceLock<Guard> = OnceLock::new();

/// The word every refusal message starts with, so a caller can tell a refusal
/// apart from an ordinary I/O failure.
pub const REFUSED: &str = "refused";

/// The two ways past a refusal an answer may lift, named in the refusal itself
/// by [`Guard::refusal_message`], because a standing allow and yolo both skip
/// the prompt and leave the message as the only thing the user sees.
const PROMPT_HINT: &str = "the user can allow it when Maki asks";

fn config_hint(access: Access) -> String {
    format!(
        "add it to [{MAKI_FILES_SECTION}].{} in {PERMISSIONS_FILE}",
        access.as_str()
    )
}

/// The one section of `permissions.toml` that is not a tool: the paths among
/// Maki's own files that the user handed to the agent. Shared so the rule that
/// closes the file, the loader that reads the section and the refusal that
/// points at it all name one thing.
pub const MAKI_FILES_SECTION: &str = "maki_files";

/// The state-dir subtrees the agent may reach, for a caller that needs to name
/// one. Read out of `STATE_ENTRIES` so it cannot drift from the rules.
pub fn open_state_subtrees() -> impl Iterator<Item = &'static str> {
    STATE_ENTRIES
        .iter()
        .filter(|(_, reach)| matches!(reach, Reach::Open))
        .map(|(name, _)| *name)
}

/// What a caller is about to do with a path. Asked for rather than assumed,
/// because the answers differ: see `Reach::ReadOnly`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Read,
    Write,
}

impl Access {
    pub const ALL: [Self; 2] = [Self::Read, Self::Write];

    /// Also the key an override is written under in `permissions.toml`, so the
    /// file and the enum cannot name the same grant differently.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    /// The inverse of [`Access::as_str`], built out of it so the two cannot
    /// disagree. A caller must refuse `None` rather than guess: guessing `Read`
    /// skips a check, guessing `Write` grants what nobody was asked about.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|access| access.as_str() == s)
    }
}

/// Why a path is refused, so a caller's message names the rule that refused
/// instead of guessing at one.
///
/// Only a message. Whether an approval may lift it is `Override`, kept apart so
/// a reason is picked for what it explains. Tied together, the approval store
/// would have to claim it holds credentials just to stay closed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    OwnState,
    Credentials,
    ApprovalStore,
    PermissionPolicy,
    PackageCode,
    StartupCode,
    Degraded,
}

impl Refusal {
    /// The agent is the one reading this, so it says what to do next rather
    /// than only what failed.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnState => {
                "Maki keeps its own state here, so this path is not reachable through its file tools"
            }
            Self::Credentials => {
                "this file holds provider credentials, so only the user may read or edit it"
            }
            Self::ApprovalStore => {
                "this file records which packages the user let Maki run as code, so only the user may read or edit it"
            }
            Self::PermissionPolicy => {
                "this file sets the limits Maki itself runs under, so it can be read but only the user may change it"
            }
            Self::PackageCode => {
                "Maki loads this as code on its next start, so it can be read but only the user may change it"
            }
            Self::StartupCode => {
                "Maki runs this file as Lua inside its own process when it next starts, so it can be read but only the user may change it"
            }
            Self::Degraded => {
                "Maki cannot tell where its own files live, so every path it has no rule for is refused"
            }
        }
    }

    /// Takes the path as the caller spelled it, since that is the one it can
    /// correct.
    pub fn message(self, path: &str) -> String {
        format!("{REFUSED}: {path}: {}", self.as_str())
    }
}

/// Whether a human answer can open what a rule refuses. Written at the rule
/// site, so nobody has to read a `Refusal` to find out how dangerous it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Override {
    /// Nothing lifts this rule. The lookup returns before overrides are read,
    /// so no entry in that list reaches these paths however it got there.
    Never,
    /// An explicit approval for one path lifts it, checked by
    /// [`Guard::stage`].
    ByApproval,
}

/// A directory the rules cover, paired with the entry table overlaid on it.
type Role<'a> = (Option<&'a Path>, &'static [(&'static str, Reach)]);

/// A rule's answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reach {
    /// Cancels the wider rule it sits inside.
    Open,
    /// Readable, never writable. Reviewing an installed package means reading
    /// it, and Maki `require`s those same files on the next start under
    /// whatever the package was approved for. The approval keys on a package's
    /// name and source, never on its contents, so a write here would be code
    /// execution nobody agreed to.
    ReadOnly(Refusal, Override),
    Closed(Refusal, Override),
    /// Closed both ways, but a person may open the read and never the write.
    /// The blanket over Maki's own directories: see the note at the top of the
    /// module for why it leans this way. Anything the agent legitimately writes
    /// has an `Open` entry of its own.
    ReadByApproval(Refusal),
}

impl Reach {
    /// Why this rule refuses `access`, and whether an approval may lift it.
    /// One match, so a caller cannot learn the reason and then guess at the
    /// policy.
    fn verdict(self, access: Access) -> Option<(Refusal, Override)> {
        match (self, access) {
            (Self::Open, _) | (Self::ReadOnly(..), Access::Read) => None,
            (Self::ReadOnly(reason, over), Access::Write) | (Self::Closed(reason, over), _) => {
                Some((reason, over))
            }
            (Self::ReadByApproval(reason), Access::Read) => Some((reason, Override::ByApproval)),
            (Self::ReadByApproval(reason), Access::Write) => Some((reason, Override::Never)),
        }
    }

    fn refusal(self, access: Access) -> Option<Refusal> {
        self.verdict(access).map(|(reason, _)| reason)
    }

    /// How strictly this rule answers `access`.
    ///
    /// One directory can serve two roles, on a legacy `~/.maki` and on stock
    /// Windows alike, so two rules can name one path. Ranking the answers
    /// settles the tie toward the stricter rule instead of toward whichever
    /// one `for_layout` happened to build first.
    fn strictness(self, access: Access) -> u8 {
        match self.verdict(access) {
            None => 0,
            Some((_, Override::ByApproval)) => 1,
            Some((_, Override::Never)) => 2,
        }
    }
}

/// Why an override was refused.
///
/// Both refusals are hard errors. A `[maki_files]` entry that cannot take
/// effect has to say so, or the user who wrote it goes on believing the path is
/// open.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum OverrideError {
    #[error("{}: {}, and no approval changes that", path.display(), reason.as_str())]
    NeverOverridable { path: PathBuf, reason: Refusal },
    #[error(
        "{}: nothing refuses {} here, so this grants nothing",
        path.display(),
        access.as_str()
    )]
    Unnecessary { path: PathBuf, access: Access },
    #[error("{}: Maki cannot tell where its own files live, so no override is honoured", path.display())]
    Degraded { path: PathBuf },
}

/// An override that passed the rules and has not taken effect yet.
///
/// [`Guard::stage`] is the only way to make one, and [`Guard::grant`] and
/// [`Guard::install_config`] the only things to do with one. So nothing reaches
/// the guard unchecked, and a caller holding several can be refused before any
/// of them is in force.
#[must_use = "a staged override opens nothing until the guard takes it"]
pub struct StagedOverride {
    key: PathBuf,
    access: Access,
}

impl StagedOverride {
    /// The file this opens, as the guard matched it rather than as the caller
    /// spelled it. Durable grants are written down this way, or an approval
    /// given for `./.maki/init.lua` in one directory would come back on the
    /// next start naming a different file.
    pub fn path(&self) -> &Path {
        &self.key
    }

    pub fn access(&self) -> Access {
        self.access
    }
}

struct Paths {
    config: PathBuf,
    data: PathBuf,
    state: PathBuf,
    logs: PathBuf,
    cache: PathBuf,
    xdg_config: PathBuf,
    /// Worked out with the rest of the layout, so every caller gets the same
    /// list for the life of the process.
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

/// Work the whole layout out once and keep it, so "where is my config" cannot
/// change while Maki runs. See `freeze`.
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

/// Pin Maki's own directories, and the rules protecting them, for the rest of
/// the process.
///
/// Call this before loading any `.env` file, project or global. Those files go
/// into the process environment, `HOME` is a key like any other in them, and
/// `HOME` is what `etcetera` answers with. Freeze first and an `.env` still
/// reaches the tools Maki starts, without moving Maki's own files out from
/// under it.
///
/// The error is worth reporting once, at startup, by whoever can print it. A
/// guard with no layout refuses every path, so the alternative is a session
/// where each file tool fails on its own and none of them mentions the one
/// environment variable behind it all.
pub fn freeze() -> Result<(), NoLayout> {
    guard();
    resolve().map(|_| ()).ok_or(NoLayout)
}

/// Maki cannot tell where its own files live, which leaves the guard refusing
/// everything. Its `Display` is what the user is told to do about it.
#[derive(Debug, Error, PartialEq, Eq)]
#[error(
    "cannot tell where your home directory is, so Maki can neither find its own files nor keep the agent out of them; set HOME to a directory you own and start again"
)]
pub struct NoLayout;

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

/// The home directory as it was when Maki started. Frozen because every other
/// answer here is built on it, and two answers in one process would mean the
/// guard protects one layout while the loaders read another.
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
/// Listing candidates creates none of them, so a caller taking the first one
/// that exists is not answered by its own asking. Empty means Maki has no idea
/// where its directories are, which is what leaves the guard degraded.
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

/// The process-wide rule set, built once from the frozen layout, so it
/// protects the same directories the loaders read.
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

/// Hand the process its rule set, for a test that owns a layout. Returns
/// whether it landed, which it only does before anything asked for the rules.
/// Maki's own startup asks in `freeze`, so the first writer is always the real
/// layout and a late caller cannot weaken a running Maki.
///
/// Tests need it because the real rules follow the developer's home directory.
pub fn install_guard(guard: Guard) -> bool {
    PROTECTION_RULES.set(guard).is_ok()
}

/// Every directory the rules are built from.
///
/// No `Default` and no partial constructor on purpose. A directory missing
/// from a layout is an open door, so every caller names every role and adding
/// one here breaks the build at each of them instead of opening a hole. The
/// cache was left out once, and the model catalog inside it picks the base URL
/// of every provider request.
///
/// `None` means this layout has nowhere to put that role, and it still has to
/// be spelled out.
pub struct Layout<'a> {
    pub state: Option<&'a Path>,
    /// Closed, with `DATA_ENTRIES` overlaid on it.
    pub data: Option<&'a Path>,
    pub cache: Option<&'a Path>,
    /// Closed: a log holds whatever the session held, prompts and tool output
    /// alike. Needs a rule of its own because `state_logs` puts it beside the
    /// state dir rather than inside it.
    pub logs: Option<&'a Path>,
    /// The user's, apart from the few files in them that hold keys.
    pub config_dirs: &'a [PathBuf],
    /// Where `~/.maki` would be. Separate from `config_dirs`, which only holds
    /// directories that exist, because the rule has to cover `~/.maki` before
    /// anyone creates it.
    pub home: Option<&'a Path>,
}

/// The paths an approval opened, and how far, split by what ends the grant.
///
/// Two lists and not one, because they answer to different sources of truth.
/// `config` mirrors `[maki_files]` and is replaced whole on every read of that
/// file, so deleting a line and reloading takes the grant away. `granted` is
/// what a live answer opened, which no file describes and only a restart drops.
#[derive(Default)]
struct Overrides {
    config: Vec<(PathBuf, Access)>,
    granted: Vec<(PathBuf, Access)>,
}

impl Overrides {
    fn entries(&self) -> impl Iterator<Item = &(PathBuf, Access)> {
        self.config.iter().chain(&self.granted)
    }
}

/// A rule set for one layout. `guard()` returns the process-wide one that
/// every caller in Maki uses, and a test builds its own for `install_guard`.
pub struct Guard {
    /// Root plus what may be done under it, in no particular order: the
    /// longest matching root decides, so a rule can be added anywhere.
    ///
    /// Immutable once built. Overrides live in their own list rather than
    /// joining this one, so an override cannot win a specificity contest
    /// against a rule that forbids it.
    rules: Vec<(PathBuf, Reach)>,
    /// Consulted only where the matching rule refused *and* said an approval is
    /// a legal answer, which is what puts a `Never` rule out of reach.
    overrides: ArcSwap<Overrides>,
    /// The config dirs of this layout, canonical. Only [`Guard::shaped`] reads
    /// them, to tell the user's own `~/.maki` from a repository's.
    config_dirs: Vec<PathBuf>,
    /// The layout is unknown, so a path no rule names could be Maki's own
    /// state and gets refused rather than allowed.
    degraded: bool,
}

impl Guard {
    /// Why `path` is refused for `access`, or `None` when the agent may have
    /// it. Resolution goes through `canonical_key`, so neither `..` nor a
    /// symlink spells a way around a rule, and a file that does not exist yet
    /// is covered before anyone creates it.
    pub fn refusal(&self, path: &Path, access: Access) -> Option<Refusal> {
        self.refusal_key(&canonical_key(path), access)
    }

    /// `refusal` for a caller that already resolved the path to a canonical
    /// key. The walkers use it because resolving is the expensive half and they
    /// do it once per subtree. A key that is not canonical asks the wrong
    /// question, so resolve with `canonical_key` or something else a symlink
    /// cannot fool.
    pub fn refusal_key(&self, key: &Path, access: Access) -> Option<Refusal> {
        let Some(reach) = self.matched(key, access) else {
            return self.degraded.then_some(Refusal::Degraded);
        };
        let (reason, over) = reach.verdict(access)?;
        // The only place an override is read. A `Never` rule and a degraded
        // guard have both already returned, so no entry in that list reaches
        // either, however it was spelled and however long it is.
        if self.degraded || over == Override::Never || !self.opened(key, access) {
            return Some(reason);
        }
        None
    }

    /// The message a file tool answers a refused path with: the reason, plus
    /// the way out where an answer is one. `None` when the agent may have the
    /// path.
    ///
    /// `as_written` is the path as the caller spelled it, since that is the one
    /// it can correct.
    pub fn refusal_message(&self, path: &Path, access: Access, as_written: &str) -> Option<String> {
        let key = canonical_key(path);
        let message = self.refusal_key(&key, access)?.message(as_written);
        Some(match self.override_candidate_key(&key, access).is_some() {
            true => format!("{message} ({}, or {})", PROMPT_HINT, config_hint(access)),
            false => message,
        })
    }

    /// The most specific rule covering `key`, overrides left out.
    ///
    /// Every matching root is a prefix of `key`, so the longest one is the most
    /// specific: `<state>/projects` beats the closed `<state>` around it,
    /// wherever it sits in the list. Ties go to the strictest answer for
    /// `access`: see [`Reach::strictness`].
    ///
    /// A shape rule answers alongside those, and the stricter of the two wins.
    /// A `.maki` directory turns up under any root, an open one included: a
    /// checkout under a config dir, a repository cloned into a subtree a plugin
    /// owns. If the placed rule always won, those four names would carry their
    /// rules everywhere except inside the directories Maki itself opened.
    fn matched(&self, key: &Path, access: Access) -> Option<Reach> {
        let placed = self
            .rules
            .iter()
            .filter(|(root, _)| key.starts_with(root))
            .max_by_key(|(root, reach)| (root.as_os_str().len(), reach.strictness(access)))
            .map(|(_, reach)| *reach);
        match (placed, self.shaped(key)) {
            (Some(placed), Some(shaped)) => Some(std::cmp::max_by_key(placed, shaped, |r| {
                r.strictness(access)
            })),
            (placed, shaped) => placed.or(shaped),
        }
    }

    /// The rule `key` carries because of its name, unless the `.maki` directory
    /// holding it is one Maki resolved as a config dir.
    ///
    /// That exception is the legacy `~/.maki`, where `init.lua` and
    /// `permissions.toml` are the user's own files and the ones they came to
    /// Maki to have edited.
    fn shaped(&self, key: &Path) -> Option<Reach> {
        let reach = shape_reach(key)?;
        let dir = key.parent()?;
        (!self.config_dirs.iter().any(|config| config == dir)).then_some(reach)
    }

    /// Whether any rule that names a place is near enough to `key` to have
    /// anything to say about it or about anything under it.
    ///
    /// A walk over an unrelated tree asks this once instead of asking about
    /// every entry it meets. That is sound because an override is only ever
    /// staged against a rule that allows one, so an override root always sits
    /// under a rule root, and because a degraded guard answers yes.
    ///
    /// Says nothing about the shape rules, which match under any root at all.
    /// A walk still owes those [`Guard::hidden_by_shape`] per entry, which is
    /// why that one costs no key and no rule scan.
    pub fn may_cover_subtree(&self, key: &Path) -> bool {
        self.degraded
            || self
                .rules
                .iter()
                .any(|(root, _)| root.starts_with(key) || key.starts_with(root))
    }

    /// Whether a walk must drop `path` because of a rule that matches by shape.
    ///
    /// Lexical, and the only guard question a walk asks without a canonical
    /// key. A walk does not follow links, so the path it yields is already the
    /// file's own, and the answer needs a file name and its parent's rather
    /// than a rule scan. Every walk asks it, even over a tree
    /// [`Guard::may_cover_subtree`] cleared, because a `.maki` directory turns
    /// up under any root.
    ///
    /// Reads only, since nothing a walk does writes, and only where no approval
    /// could ever lift the refusal. The override list is keyed on canonical
    /// paths, so a lexical answer must not contradict it. A shape rule a person
    /// can open is left to the per-entry lookup, which holds the key. Erring
    /// that way costs a path in a listing the user could have approved, never
    /// one they did.
    pub fn hidden_by_shape(&self, path: &Path) -> bool {
        shape_reach(path)
            .and_then(|reach| reach.verdict(Access::Read))
            .is_some_and(|(_, over)| over == Override::Never)
    }

    /// Whether an approval already opened `key` for `access`. A write grant
    /// implies read: no reach is writable and not readable, so there is no
    /// third state for an override to be in.
    fn opened(&self, key: &Path, access: Access) -> bool {
        self.overrides.load().entries().any(|(root, granted)| {
            (access == Access::Read || *granted == Access::Write) && key.starts_with(root)
        })
    }

    /// Whether an approval is a legal answer to a refusal of `path`, and the
    /// reason it would lift. `None` where nothing refuses, where only the user
    /// may ever have it, and where an approval already landed.
    ///
    /// The prompt, the refusal message and the site that records the answer all
    /// ask here, so they cannot disagree and leave a path approved but still
    /// refused.
    pub fn override_candidate(&self, path: &Path, access: Access) -> Option<Refusal> {
        self.override_candidate_key(&canonical_key(path), access)
    }

    fn override_candidate_key(&self, key: &Path, access: Access) -> Option<Refusal> {
        if self.degraded {
            return None;
        }
        match self.matched(key, access)?.verdict(access)? {
            (reason, Override::ByApproval) if !self.opened(key, access) => Some(reason),
            _ => None,
        }
    }

    /// Checks one override against the rules and answers with it, unapplied.
    ///
    /// The only place the rules are consulted for an override, so a live answer
    /// and a `[maki_files]` line get the same checks. Two sets of checks would
    /// mean grants that work this session and vanish after a restart, and a bug
    /// that changes its symptom when you restart is the worst kind to chase.
    ///
    /// `access` is what the grant covers: `Read` makes the path readable,
    /// `Write` makes it readable and writable.
    pub fn stage(&self, path: &Path, access: Access) -> Result<StagedOverride, OverrideError> {
        let key = canonical_key(path);
        if self.degraded {
            return Err(OverrideError::Degraded { path: key });
        }
        // Asked of the base rule rather than of `override_candidate`, so
        // replaying an entry already in force is not an error: a startup that
        // loads two entries covering one path must behave like the approval
        // that wrote them.
        match self.matched(&key, access).and_then(|r| r.verdict(access)) {
            Some((_, Override::ByApproval)) => Ok(StagedOverride { key, access }),
            Some((reason, Override::Never)) => {
                Err(OverrideError::NeverOverridable { path: key, reason })
            }
            None => Err(OverrideError::Unnecessary { path: key, access }),
        }
    }

    /// Opens what a live human answer opened, for the rest of the session.
    ///
    /// Append-only, because nothing on disk describes these and there is
    /// nothing to compare them against later. The durable half of an "always"
    /// answer is a `[maki_files]` entry, which comes back through
    /// [`Guard::install_config`] on the next start.
    pub fn grant(&self, staged: impl IntoIterator<Item = StagedOverride>) {
        let staged: Vec<StagedOverride> = staged.into_iter().collect();
        if staged.is_empty() {
            return;
        }
        self.overrides.rcu(|current| Overrides {
            config: current.config.clone(),
            granted: merged(&current.granted, &staged),
        });
        log_opened("an approval opened one of Maki's own files", &staged);
    }

    /// Replaces what `[maki_files]` opened with what it says now.
    ///
    /// Replaced and not appended, so a line the user deleted before `/reload`
    /// closes the path again instead of staying open until the process ends
    /// with no file describing why.
    ///
    /// All of them or none, since the caller stages the whole section first. A
    /// file with one bad line leaves the previous config in force rather than
    /// half a section nothing describes.
    pub fn install_config(&self, staged: impl IntoIterator<Item = StagedOverride>) {
        let staged: Vec<StagedOverride> = staged.into_iter().collect();
        self.overrides.rcu(|current| Overrides {
            config: merged(&[], &staged),
            granted: current.granted.clone(),
        });
        log_opened(
            "the user's own permissions.toml opened one of Maki's files",
            &staged,
        );
    }

    /// Stage one override and grant it, for the live approval that has
    /// nothing to batch it with.
    pub fn add_override(&self, path: &Path, access: Access) -> Result<(), OverrideError> {
        self.grant([self.stage(path, access)?]);
        Ok(())
    }

    pub fn is_unreachable(&self, path: &Path, access: Access) -> bool {
        self.refusal(path, access).is_some()
    }

    pub fn is_unreachable_key(&self, key: &Path, access: Access) -> bool {
        self.refusal_key(key, access).is_some()
    }

    /// Whether a directory walk may drop `key` and never look inside it.
    ///
    /// Not the same question as `refusal`, because open subtrees sit inside
    /// closed ones and a walk pruning at the closed parent never reaches them.
    /// Pruning the state dir hid the memory notes, pruning the data dir hid
    /// every installed package, and on a legacy `~/.maki` it hid the user's own
    /// skills, so one file was findable or not depending on where the search
    /// started. A closed directory is skipped only when nothing under it is
    /// readable, and skipping stays an optimisation: a walk that descends still
    /// drops each unreadable entry it meets.
    ///
    /// An override opens a subtree exactly as an open rule does, so it stops a
    /// prune for the same reason.
    pub fn may_skip_key(&self, key: &Path) -> bool {
        self.is_unreachable_key(key, Access::Read)
            && !self
                .rules
                .iter()
                .any(|(root, reach)| reach.refusal(Access::Read).is_none() && root.starts_with(key))
            && !self
                .overrides
                .load()
                .entries()
                .any(|(root, _)| root.starts_with(key))
    }

    /// Whether removing `path` and everything under it would take a file the
    /// agent may not write.
    ///
    /// Every rule names a root, and a caller standing above that root never has
    /// to name a closed file to destroy it: the parent of the state dir is an
    /// ordinary path, and removing it takes the credentials.
    ///
    /// `path` itself and each root go to [`Guard::refusal_key`], the same
    /// question a write to either would ask, rather than being read out of the
    /// rule here. Open rules answer `None` and drop out, an approval that
    /// opened a root for writing is honoured without this knowing overrides
    /// exist, and there is no second reading of the tables to drift from the
    /// first.
    pub fn contains_unwritable(&self, path: &Path) -> bool {
        let key = canonical_key(path);
        self.refusal_key(&key, Access::Write).is_some()
            || self.rules.iter().any(|(root, _)| {
                root.starts_with(&key) && self.refusal_key(root, Access::Write).is_some()
            })
    }

    /// Every role is laid out the same way: the blanket reach for the
    /// directory, then its own entry table on top. `CONFIG_ENTRIES` is the one
    /// table with no blanket under it, because the rest of a config dir is the
    /// user's own work.
    ///
    /// `~/.maki` is ruled in whether or not it exists today, the one rule not
    /// taken from the resolved layout. Creating that directory is what picks
    /// the layout for the next start, so waiting for it to appear would put the
    /// guard one `mkdir` behind the agent.
    ///
    /// An unknown state dir refuses everything, since Maki cannot name the
    /// files it has to keep. `guard()` warns so the refusals have an
    /// explanation.
    pub fn for_layout(layout: &Layout<'_>) -> Self {
        const OWN: Reach = Reach::ReadByApproval(Refusal::OwnState);
        let Layout {
            state,
            data,
            cache,
            logs,
            config_dirs,
            home,
        } = *layout;
        let legacy = home.map(|h| h.join(MAKI_DIR));
        // `~/.maki` gets no entry table: when it is the state dir the state
        // role already covers it, and when it is not, nothing is known to live
        // under it.
        let roles: [Role<'_>; 5] = [
            (state, &STATE_ENTRIES),
            (legacy.as_deref(), &[]),
            (cache, &[]),
            (logs, &[]),
            (data, &DATA_ENTRIES),
        ];
        let own = roles.into_iter().flat_map(|(dir, entries)| {
            dir.into_iter().flat_map(move |dir| {
                std::iter::once(rule(dir, OWN)).chain(
                    entries
                        .iter()
                        .map(move |(name, reach)| rule(&dir.join(name), *reach)),
                )
            })
        });
        let config = config_dirs.iter().flat_map(|dir| {
            CONFIG_ENTRIES
                .iter()
                .map(move |(name, reach)| rule(&dir.join(name), *reach))
        });
        Self {
            rules: own.chain(config).collect(),
            overrides: ArcSwap::from_pointee(Overrides::default()),
            config_dirs: config_dirs.iter().map(|dir| canonical_key(dir)).collect(),
            degraded: state.is_none(),
        }
    }
}

fn rule(path: &Path, reach: Reach) -> (PathBuf, Reach) {
    (canonical_key(path), reach)
}

/// `base` plus every staged entry it does not already hold. Duplicates are
/// dropped rather than stacked, so one path opened twice is one entry and the
/// list cannot grow without bound across reloads.
fn merged(base: &[(PathBuf, Access)], staged: &[StagedOverride]) -> Vec<(PathBuf, Access)> {
    let mut next = base.to_vec();
    for entry in staged {
        let entry = (entry.key.clone(), entry.access);
        if !next.contains(&entry) {
            next.push(entry);
        }
    }
    next
}

fn log_opened(message: &'static str, staged: &[StagedOverride]) {
    for entry in staged {
        tracing::info!(
            path = %entry.key.display(),
            access = entry.access.as_str(),
            "{message}"
        );
    }
}

/// The rule a path carries because of what it is called and what holds it,
/// rather than because of where Maki found it. See [`MAKI_DIR_ENTRIES`].
///
/// A name comparison and no allocation, because every entry of every walk asks
/// this.
fn shape_reach(path: &Path) -> Option<Reach> {
    let name = path.file_name()?;
    if path.parent()?.file_name()? != MAKI_DIR {
        return None;
    }
    MAKI_DIR_ENTRIES
        .iter()
        .find(|(entry, _)| name == *entry)
        .map(|(_, reach)| *reach)
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
    use std::sync::Arc;

    use test_case::test_case;

    use super::*;
    use crate::StateDir;

    const KEYED_FILE: &str = "f.rs";
    const SUBDIR: &str = "sub";
    const CONFIG_FILE: &str = "config.toml";
    /// A name no entry table claims, so the blanket rule of a role is the one
    /// that answers for it.
    const NOTE_FILE: &str = "note.md";
    const SIBLING_SUFFIX: &str = "-old";
    const SKILLS_DIR: &str = "skills";
    const OPEN_STATE_SUBTREE: &str = "plans";
    const CLONE_DIR: &str = "clone";
    const PROVIDER: &str = "anthropic";
    const XDG_CONFIG_REL: &str = ".config";
    const WINDOWS_APPDATA_REL: &str = "AppData/Roaming";
    const STATE_ROLE: &str = "state";
    const DATA_ROLE: &str = "data";
    const CONFIG_ROLE: &str = "config";
    const CACHE_ROLE: &str = "cache";
    const LOGS_ROLE: &str = "logs";
    const PROJECT_ROLE: &str = "project";
    const HOME_ROLE: &str = "home";

    const FAIL_CLOSED: &str = "a guard that cannot be built must refuse, not allow";
    const POLICY_ESCALATION: &str = "an approval is a legal answer exactly where the row says so";
    const OVERRIDE_IS_LEGAL: &str = "the blanket rule of this role takes an approval to read";
    const SHAPE_HOLDS: &str = "these four files decide what Maki does with the folder around them";
    const ONLY_THE_NAMES: &str = "the directory itself is the repository's, only the four are not";
    const PRUNE_HIDES_THE_OVERRIDE: &str = "a walk that prunes here never reaches what was opened";

    /// The two ways one directory ends up serving two roles.
    #[derive(Clone, Copy)]
    enum Collapsed {
        /// `~/.maki` exists, so every role collapses onto it.
        LegacyHome,
        /// `etcetera` answers `%APPDATA%\maki` for config and data, and has no
        /// state dir to offer, so Maki puts its state there too.
        StockWindows,
    }

    /// Spelled out rather than defaulted, because `Layout` has no `Default` on
    /// purpose: see the note on it.
    fn guard_from(
        state: Option<&Path>,
        data: Option<&Path>,
        config_dirs: &[PathBuf],
        home: Option<&Path>,
    ) -> Guard {
        Guard::for_layout(&Layout {
            state,
            data,
            config_dirs,
            home,
            cache: None,
            logs: None,
        })
    }

    /// Builds the rules the way `guard()` does, from a home directory shaped
    /// like the platform in question, so the test asks what the process asks
    /// rather than a hand-written stand-in.
    fn collapsed_guard(home: &Path, shape: Collapsed) -> (Guard, PathBuf) {
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
        let guard = guard_from(Some(&shared), Some(&shared), &config_dirs, Some(home));
        (guard, shared)
    }

    /// One of the state-dir subtrees a feature depends on, for a case that only
    /// needs a path the agent may have.
    fn open_subtree() -> &'static str {
        open_state_subtrees()
            .next()
            .expect("the state dir keeps at least one open subtree")
    }

    /// Asks the code that stores credentials where it puts them, so moving
    /// them cannot move them out from under the rule without this failing.
    fn credentials_in(state: &Path) -> PathBuf {
        crate::auth::auth_path(&StateDir::from_path(state.to_path_buf()), PROVIDER)
    }

    /// A guard that cannot find the state dir used to answer "no rule applies"
    /// for the credentials, which reads the same as "help yourself". A removal
    /// gets the same answer, since it cannot name the roots it would take.
    #[test_case(credentials_in; "credentials")]
    #[test_case(|state| state.join(NOTE_FILE); "a_state_file")]
    #[test_case(|_| PathBuf::from("/etc").join(NOTE_FILE); "any_other_path")]
    fn an_unknown_state_dir_refuses(spell: fn(&Path) -> PathBuf) {
        let would_be_state = tempfile::TempDir::new().unwrap();
        let guard = guard_from(None, None, &[], None);
        let path = spell(would_be_state.path());

        assert!(guard.is_unreachable(&path, Access::Read), "{FAIL_CLOSED}");
        assert!(guard.contains_unwritable(&path), "{FAIL_CLOSED}");
    }

    /// Plan mode allocates a path and has the agent write the plan there with
    /// the `write` tool, so closing `plans` fails quietly rather than loudly:
    /// the write is refused and the trigger waiting for it never fires. The
    /// path comes from the code that allocates it, so moving plans elsewhere
    /// cannot move them out from under this rule in silence.
    #[test]
    fn the_agent_can_write_the_plan_it_was_asked_for() {
        let state = tempfile::TempDir::new().unwrap();
        let guard = guard_from(Some(state.path()), None, &[], None);
        let dir = StateDir::from_path(state.path().to_path_buf());
        let plan = crate::plans::new_plan_path(&dir).unwrap();

        assert!(!guard.is_unreachable(&plan, Access::Write));
    }

    /// What a walk may skip, which is not the same as what it may read. The
    /// rules open subtrees back up inside closed ones, and a walk that prunes
    /// at the closed parent never reaches them: the same note was findable
    /// from inside the state dir and missing from a search one directory up.
    #[test_case(|state, _| state.to_path_buf(), false; "a_state_dir_holding_open_subtrees")]
    #[test_case(|state, _| state.join(open_subtree()), false; "an_open_subtree")]
    #[test_case(|_, data| data.to_path_buf(), false; "a_data_dir_holding_packages")]
    #[test_case(|_, data| data.join(SITE_DIR), false; "a_package_checkout")]
    #[test_case(|state, _| credentials_in(state), true; "a_closed_state_file")]
    fn a_walk_only_skips_what_holds_nothing_readable(
        spell: fn(&Path, &Path) -> PathBuf,
        expected: bool,
    ) {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join(STATE_ROLE);
        let data = root.path().join(DATA_ROLE);
        let guard = guard_from(Some(&state), Some(&data), &[], None);

        assert_eq!(
            guard.may_skip_key(&canonical_key(&spell(&state, &data))),
            expected
        );
    }

    /// A walk asks this once and, where the answer is no, stops asking about the
    /// entries: a project is almost never anywhere near Maki's own directories,
    /// and a full-repository search asks per entry otherwise. So the answer has
    /// to be yes for every root that could hold something a rule names, from
    /// above and from inside alike.
    #[test_case(|root, _| root.to_path_buf(), true; "a_root_holding_makis_own_directories")]
    #[test_case(|_, state| state.to_path_buf(), true; "the_state_dir_itself")]
    #[test_case(|_, state| state.join(open_subtree()), true; "a_root_inside_an_open_subtree")]
    #[test_case(|root, _| root.join(STATE_ROLE.to_owned() + SIBLING_SUFFIX), false; "a_name_that_only_starts_the_same")]
    fn a_walk_asks_per_entry_only_near_makis_own_files(
        spell: fn(&Path, &Path) -> PathBuf,
        expected: bool,
    ) {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join(STATE_ROLE);
        let guard = guard_from(Some(&state), None, &[], None);

        assert_eq!(
            guard.may_cover_subtree(&canonical_key(&spell(root.path(), &state))),
            expected
        );
    }

    /// The refusal is all the user gets wherever the prompt never runs, which a
    /// standing allow for the tool and yolo both arrange, so it names the way
    /// out where an answer is one and offers nothing where no answer is.
    #[test_case(|state, _| state.join(NOTE_FILE), Access::Read, true; "a_state_file_an_answer_opens")]
    #[test_case(|_, config| config.join(ENV_FILE), Access::Read, false; "the_credentials")]
    #[test_case(|_, config| config.join(PERMISSIONS_FILE), Access::Write, false; "the_permission_policy")]
    fn a_refusal_names_the_way_out_where_an_answer_is_one(
        spell: fn(&Path, &Path) -> PathBuf,
        access: Access,
        liftable: bool,
    ) {
        let state = tempfile::TempDir::new().unwrap();
        let config = tempfile::TempDir::new().unwrap();
        let guard = guard_from(
            Some(state.path()),
            None,
            &[config.path().to_path_buf()],
            None,
        );
        let path = spell(state.path(), config.path());
        let spelled = path.to_string_lossy().into_owned();

        let message = guard
            .refusal_message(&path, access, &spelled)
            .expect("the rules refuse this path");

        assert!(
            message.starts_with(REFUSED) && message.contains(&spelled),
            "{message}"
        );
        assert_eq!(
            message.contains(MAKI_FILES_SECTION),
            liftable,
            "{message}: the way out belongs in the refusal wherever an answer is one"
        );
    }

    /// Creating `~/.maki` is what picks the layout for the next start, so the
    /// rule has to be in place before the directory is. Here the state lives
    /// somewhere else entirely, which is the ordinary setup where the hole
    /// used to open: nothing named `~/.maki`, so the agent could write into it
    /// and own the following session.
    #[test]
    fn a_legacy_home_is_ruled_in_before_it_exists() {
        let home = tempfile::TempDir::new().unwrap();
        let state = tempfile::TempDir::new().unwrap();
        let xdg = home.path().join(XDG_CONFIG_REL).join(APP_NAME);
        let config_dirs = config_search_dirs_from(Some(home.path()), Some(&xdg));
        let guard = guard_from(Some(state.path()), None, &config_dirs, Some(home.path()));
        let legacy = home.path().join(MAKI_DIR);

        assert!(
            !legacy.exists(),
            "the point of the rule is a directory that is not there yet"
        );
        assert!(
            guard.is_unreachable(&legacy, Access::Write),
            "the directory itself decides the layout, so it cannot be created"
        );
        assert!(guard.is_unreachable(&legacy.join(NOTE_FILE), Access::Read));
    }

    /// One directory in two roles, which happens on the legacy `~/.maki` and
    /// on a stock Windows install alike. The state half stays closed, the
    /// content the user works on is named, and the files holding keys are
    /// closed for the same reason they are closed anywhere else, so nobody
    /// loses their skills and nobody gains a look at a key.
    #[test_case(Collapsed::LegacyHome; "legacy_home")]
    #[test_case(Collapsed::StockWindows; "stock_windows")]
    fn a_shared_directory_closes_state_and_keeps_config(shape: Collapsed) {
        let home = tempfile::TempDir::new().unwrap();
        let (guard, shared) = collapsed_guard(home.path(), shape);
        let expectations = [
            (NOTE_FILE, Some(Refusal::OwnState)),
            (ENV_FILE, Some(Refusal::Credentials)),
            (PROVIDERS_FILE, Some(Refusal::Credentials)),
            (PERMISSIONS_FILE, None),
            (INIT_LUA, None),
            (SKILLS_DIR, None),
        ];

        for (rel, expected) in expectations {
            assert_eq!(
                guard.refusal(&shared.join(rel), Access::Read),
                expected,
                "{rel}"
            );
        }
        assert!(guard.is_unreachable(&credentials_in(&shared), Access::Read));
    }

    /// The prune that hid the user's own skills. In a collapsed layout the
    /// directory a walk would drop is Maki's state and the user's config at
    /// once, so stopping at it loses the skills, commands and Lua modules kept
    /// inside, and the same file was findable from within that directory and
    /// missing from a search one level up. The auth dir under it holds nothing
    /// readable and is still dropped whole, so a guard that gave up on pruning
    /// altogether fails here rather than passing.
    #[test_case(Collapsed::LegacyHome; "legacy_home")]
    #[test_case(Collapsed::StockWindows; "stock_windows")]
    fn a_walk_descends_into_a_shared_directory(shape: Collapsed) {
        let home = tempfile::TempDir::new().unwrap();
        let (guard, shared) = collapsed_guard(home.path(), shape);
        let auth = credentials_in(&shared).parent().unwrap().to_path_buf();

        assert!(
            !guard.may_skip_key(&canonical_key(&shared)),
            "the user's own config lives in here, so a walk cannot drop it"
        );
        assert!(
            guard.may_skip_key(&canonical_key(&auth)),
            "nothing under the auth dir is readable, so a walk still drops it whole"
        );
    }

    /// `install_guard` hands the whole process its rules, and Maki's own
    /// startup asks for them in `freeze`. A second caller landing afterwards
    /// would let a plugin or a test hand a running Maki a layout naming none of
    /// its directories, and the credentials would be readable through the front
    /// door.
    #[test]
    fn the_rules_cannot_be_replaced_once_they_are_in_force() {
        let elsewhere = tempfile::TempDir::new().unwrap();
        guard();

        assert!(
            !install_guard(guard_from(Some(elsewhere.path()), None, &[], None)),
            "the first writer wins, and that writer is the real layout"
        );
        assert!(
            !guard().is_unreachable(&elsewhere.path().join(NOTE_FILE), Access::Read),
            "a tempdir is nobody's state dir, so the late rules took hold"
        );
    }

    /// A rule names a directory, and the directory beside it is not inside it.
    /// Matching by the text of a path rather than by its components would
    /// close `<state>-old` along with `<state>`, and that one is the user's
    /// own backup, refused with a message about Maki's state.
    #[test_case(STATE_ROLE; "beside_the_state_dir")]
    #[test_case(DATA_ROLE; "beside_the_data_dir")]
    #[test_case(CACHE_ROLE; "beside_the_cache_dir")]
    fn a_directory_next_to_a_closed_root_is_the_users(role: &str) {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join(STATE_ROLE);
        let data = root.path().join(DATA_ROLE);
        let cache = root.path().join(CACHE_ROLE);
        let guard = Guard::for_layout(&Layout {
            state: Some(&state),
            data: Some(&data),
            cache: Some(&cache),
            logs: None,
            config_dirs: &[],
            home: None,
        });
        let sibling = root.path().join(role.to_owned() + SIBLING_SUFFIX);

        assert!(!guard.is_unreachable(&sibling.join(NOTE_FILE), Access::Write));
        assert!(
            !guard.contains_unwritable(&sibling),
            "removing it takes nothing of Maki's with it"
        );
    }

    /// A recursive removal never names the files it destroys, so asking about
    /// the path alone answers the wrong question: the parent of the state dir
    /// is an ordinary path, and deleting it takes the credentials.
    #[test_case(|state, _, _| state.parent().unwrap().to_path_buf(), true; "above_the_state_dir")]
    #[test_case(|state, _, _| state.to_path_buf(), true; "the_state_dir")]
    #[test_case(|_, data, _| data.to_path_buf(), true; "the_data_dir")]
    #[test_case(|state, _, _| state.join(open_subtree()), false; "an_open_subtree")]
    #[test_case(|_, data, _| data.join(SITE_DIR), true; "a_package_checkout")]
    #[test_case(|_, _, config| config.join(INIT_LUA), false; "an_open_config_file")]
    #[test_case(|_, _, config| config.to_path_buf(), true; "a_config_dir_holding_keys")]
    fn a_recursive_removal_sees_what_is_under_it(
        spell: fn(&Path, &Path, &Path) -> PathBuf,
        expected: bool,
    ) {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join(STATE_ROLE);
        let data = root.path().join(DATA_ROLE);
        let config = root.path().join(CONFIG_ROLE);
        let guard = guard_from(
            Some(&state),
            Some(&data),
            std::slice::from_ref(&config),
            None,
        );

        assert_eq!(
            guard.contains_unwritable(&spell(&state, &data, &config)),
            expected
        );
    }

    /// Every directory the rules are built from, in one tempdir, so a case can
    /// name a path in any role and still ask the guard `for_layout` builds.
    struct Roles {
        _root: tempfile::TempDir,
        state: PathBuf,
        data: PathBuf,
        cache: PathBuf,
        logs: PathBuf,
        config: PathBuf,
        home: PathBuf,
        /// No role in the layout, and that is the point: the `.maki` rules hold
        /// for a directory Maki resolved nothing about.
        project: PathBuf,
    }

    impl Roles {
        fn new() -> Self {
            let root = tempfile::TempDir::new().unwrap();
            let at = |role: &str| root.path().join(role);
            Self {
                state: at(STATE_ROLE),
                data: at(DATA_ROLE),
                cache: at(CACHE_ROLE),
                logs: at(LOGS_ROLE),
                config: at(CONFIG_ROLE),
                home: at(HOME_ROLE),
                project: at(PROJECT_ROLE),
                _root: root,
            }
        }

        /// A rule set of its own per case, so an override one case records
        /// cannot decide the next one's answer.
        fn guard(&self) -> Guard {
            Guard::for_layout(&Layout {
                state: Some(&self.state),
                data: Some(&self.data),
                cache: Some(&self.cache),
                logs: Some(&self.logs),
                config_dirs: std::slice::from_ref(&self.config),
                home: Some(&self.home),
            })
        }

        fn dir(&self, role: Role) -> PathBuf {
            match role {
                Role::State => self.state.clone(),
                Role::Data => self.data.clone(),
                Role::Cache => self.cache.clone(),
                Role::Logs => self.logs.clone(),
                Role::MakiDir => self.project.join(MAKI_DIR),
                Role::Legacy => self.home.join(MAKI_DIR),
                Role::Config => self.config.clone(),
            }
        }
    }

    /// A directory the rules cover, and the entry table overlaid on it.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Role {
        State,
        Data,
        Cache,
        Logs,
        Legacy,
        Config,
        MakiDir,
    }

    impl Role {
        const ALL: [Role; 7] = [
            Role::State,
            Role::Data,
            Role::Cache,
            Role::Logs,
            Role::Legacy,
            Role::Config,
            Role::MakiDir,
        ];

        fn entries(self) -> &'static [(&'static str, Reach)] {
            match self {
                Role::State => &STATE_ENTRIES,
                Role::Data => &DATA_ENTRIES,
                Role::Config => &CONFIG_ENTRIES,
                Role::MakiDir => &MAKI_DIR_ENTRIES,
                Role::Cache | Role::Logs | Role::Legacy => &[],
            }
        }
    }

    /// What the agent may do with one of Maki's own paths, said in the terms the
    /// feature is described in rather than in the terms the rule is written in.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Policy {
        /// Reachable with nobody asked.
        Free,
        /// Readable already, and no answer makes it writable.
        ReadOnlyForever,
        /// Readable already, and writable once the user approves this one path.
        WriteEscalatable,
        /// Refused both ways, and the user can approve the read but never the
        /// write.
        ReadEscalatable,
        /// Refused either way, and no answer changes that.
        Sealed,
    }

    /// Every rule, and what it promises. The blanket rule of each role is the
    /// row naming `NOTE_FILE`, a name no entry table claims.
    ///
    /// The row is the promise, so it is worth writing down twice: a rule whose
    /// `Override` flips silently turns an escalation into a hole here.
    const RULE_POLICIES: [(Role, &str, Policy); 31] = [
        (Role::State, NOTE_FILE, Policy::ReadEscalatable),
        (Role::State, crate::auth::AUTH_DIR, Policy::Sealed),
        (Role::State, APPROVALS_FILE, Policy::Sealed),
        (Role::State, "projects", Policy::Free),
        (Role::State, "docs", Policy::Free),
        (Role::State, OPEN_STATE_SUBTREE, Policy::Free),
        (Role::State, PLUGIN_STATE_DIR, Policy::Free),
        (Role::Data, NOTE_FILE, Policy::ReadEscalatable),
        (Role::Data, SITE_DIR, Policy::ReadOnlyForever),
        (Role::Cache, NOTE_FILE, Policy::ReadEscalatable),
        (Role::Logs, NOTE_FILE, Policy::ReadEscalatable),
        (Role::Legacy, NOTE_FILE, Policy::ReadEscalatable),
        (Role::MakiDir, NOTE_FILE, Policy::Free),
        (Role::MakiDir, ENV_FILE, Policy::Sealed),
        (Role::MakiDir, MCP_FILE, Policy::Sealed),
        (Role::MakiDir, PERMISSIONS_FILE, Policy::ReadOnlyForever),
        (Role::MakiDir, INIT_LUA, Policy::WriteEscalatable),
        (Role::Config, NOTE_FILE, Policy::Free),
        (Role::Config, ENV_FILE, Policy::Sealed),
        (Role::Config, PROVIDERS_FILE, Policy::Sealed),
        (Role::Config, MCP_FILE, Policy::Sealed),
        (Role::Config, PERMISSIONS_FILE, Policy::ReadOnlyForever),
        (Role::Config, CONFIG_FILE, Policy::Free),
        (Role::Config, INIT_LUA, Policy::Free),
        (Role::Config, "lua", Policy::Free),
        (Role::Config, "pack-lock.json", Policy::Free),
        (Role::Config, "providers", Policy::Free),
        (Role::Config, "AGENTS.md", Policy::Free),
        (Role::Config, SKILLS_DIR, Policy::Free),
        (Role::Config, "commands", Policy::Free),
        (Role::Config, "themes", Policy::Free),
    ];

    /// Whether the guard refuses `access` before anyone is asked, and whether an
    /// approval is a legal answer to that refusal.
    fn expectations(policy: Policy, access: Access) -> (bool, bool) {
        match (policy, access) {
            (Policy::Free, _)
            | (Policy::ReadOnlyForever | Policy::WriteEscalatable, Access::Read) => (false, false),
            (Policy::ReadOnlyForever, Access::Write)
            | (Policy::Sealed, _)
            | (Policy::ReadEscalatable, Access::Write) => (true, false),
            (Policy::WriteEscalatable, Access::Write) | (Policy::ReadEscalatable, Access::Read) => {
                (true, true)
            }
        }
    }

    /// Every rule the layout lays down, against the promise `RULE_POLICIES`
    /// makes for it, with and without an approval. A checklist of hand-picked
    /// paths is what rots, so this walks the tables themselves.
    #[test]
    fn every_rule_keeps_the_promise_its_row_makes() {
        let roles = Roles::new();
        for (role, rel, policy) in RULE_POLICIES {
            let path = roles.dir(role).join(rel);
            for access in Access::ALL {
                let (refused, escalatable) = expectations(policy, access);
                let guard = roles.guard();
                let at = format!("{role:?}/{rel} {}", access.as_str());

                assert_eq!(
                    guard.is_unreachable(&path, access),
                    refused,
                    "{at}: the rule refuses what its row says it refuses"
                );
                assert_eq!(
                    guard.override_candidate(&path, access).is_some(),
                    escalatable,
                    "{at}: {POLICY_ESCALATION}"
                );

                let added = guard.add_override(&path, access);
                if escalatable {
                    added.expect(POLICY_ESCALATION);
                    assert!(
                        !guard.is_unreachable(&path, access),
                        "{at}: the approval the user gave has to reach the file"
                    );
                    assert_eq!(
                        guard.override_candidate(&path, access),
                        None,
                        "{at}: an approval already in force must not ask again"
                    );
                    if access == Access::Read {
                        assert!(
                            guard.is_unreachable(&path, Access::Write),
                            "{at}: a read grant is not a write grant"
                        );
                    }
                } else if refused {
                    assert!(
                        matches!(added, Err(OverrideError::NeverOverridable { .. })),
                        "{at}: only the user may ever have this, so an override is a config error"
                    );
                } else {
                    assert!(
                        matches!(added, Err(OverrideError::Unnecessary { .. })),
                        "{at}: nothing refuses this, so an override grants nothing"
                    );
                }
            }
        }
    }

    /// The table above is worth something only if it covers every rule, so both
    /// directions are checked: a rule with no row goes untested, and a row
    /// naming no rule is a promise about a rule that moved.
    #[test]
    fn the_policy_table_covers_every_rule() {
        let named = |role: Role, rel: &str| {
            RULE_POLICIES
                .iter()
                .any(|(r, name, _)| *r == role && *name == rel)
        };
        for role in Role::ALL {
            assert!(
                named(role, NOTE_FILE),
                "{role:?}: no row covers the blanket rule of this role"
            );
            for (name, _) in role.entries() {
                assert!(named(role, name), "{role:?}/{name}: this rule has no row");
            }
        }
        for (role, rel, _) in RULE_POLICIES {
            assert!(
                rel == NOTE_FILE || role.entries().iter().any(|(name, _)| *name == rel),
                "{role:?}/{rel}: this row promises something about a rule that moved"
            );
        }
    }

    /// A `.maki` rule holds under a directory nobody told the guard about,
    /// which is every checkout but the one project Maki resolved: a sibling
    /// clone, a vendored copy, a repository the agent cloned into `/tmp`.
    #[test_case(ENV_FILE, Access::Read; "a_repositorys_env")]
    #[test_case(MCP_FILE, Access::Write; "the_servers_it_starts")]
    #[test_case(PERMISSIONS_FILE, Access::Write; "the_rules_it_runs_under")]
    fn a_maki_dir_carries_its_rules_wherever_it_sits(name: &str, access: Access) {
        let elsewhere = tempfile::TempDir::new().unwrap();
        let guard = guard_from(Some(&elsewhere.path().join(STATE_ROLE)), None, &[], None);
        let maki_dir = elsewhere.path().join(CLONE_DIR).join(MAKI_DIR);

        assert!(
            guard.is_unreachable(&maki_dir.join(name), access),
            "{SHAPE_HOLDS}"
        );
        assert!(!guard.is_unreachable(&maki_dir, access), "{ONLY_THE_NAMES}");
    }

    /// "Wherever it sits" has to include the directories Maki itself opened,
    /// or the four names carry their rules everywhere except where a checkout
    /// is most likely to end up under one: a skill or a Lua module in a config
    /// dir, a repository cloned into the subtree a plugin owns.
    #[test_case(Role::Config, SKILLS_DIR; "a_checkout_under_a_config_dir")]
    #[test_case(Role::State, OPEN_STATE_SUBTREE; "a_checkout_in_an_open_state_subtree")]
    fn a_maki_dir_carries_its_rules_inside_an_open_subtree(role: Role, open: &str) {
        let roles = Roles::new();
        let guard = roles.guard();
        let clone = roles.dir(role).join(open).join(CLONE_DIR);

        assert!(
            !guard.is_unreachable(&clone.join(NOTE_FILE), Access::Read),
            "{ONLY_THE_NAMES}"
        );
        assert!(
            guard.is_unreachable(&clone.join(MAKI_DIR).join(ENV_FILE), Access::Read),
            "{SHAPE_HOLDS}"
        );
    }

    /// The user's own config dir can be a `.maki` directory, and then the two
    /// tables meet on one file. The rule written against the layout Maki
    /// resolved is the answer, or a legacy user's own `init.lua` would fall
    /// under the rule meant for a repository's.
    #[test]
    fn a_legacy_config_dir_keeps_its_own_answer() {
        let home = tempfile::TempDir::new().unwrap();
        let legacy = home.path().join(MAKI_DIR);
        let guard = guard_from(
            Some(&home.path().join(STATE_ROLE)),
            None,
            std::slice::from_ref(&legacy),
            Some(home.path()),
        );

        assert!(
            !guard.is_unreachable(&legacy.join(INIT_LUA), Access::Write),
            "a rule written against the resolved layout is the answer where there is one"
        );
        assert!(
            guard.is_unreachable(&legacy.join(ENV_FILE), Access::Read),
            "the credentials are closed under either table"
        );
    }

    /// What a walk drops without building a key for it. Only the names no
    /// approval could open, since the override list is keyed on canonical paths
    /// and this answer is lexical.
    #[test_case(ENV_FILE => true; "the_credentials")]
    #[test_case(MCP_FILE => true; "the_servers")]
    #[test_case(PERMISSIONS_FILE => false; "readable_so_a_search_may_list_it")]
    #[test_case(INIT_LUA => false; "an_approval_can_open_this_one")]
    #[test_case(NOTE_FILE => false; "anything_else_in_there")]
    fn a_walk_drops_the_names_no_answer_opens(name: &str) -> bool {
        let elsewhere = tempfile::TempDir::new().unwrap();
        let guard = guard_from(Some(&elsewhere.path().join(STATE_ROLE)), None, &[], None);

        guard.hidden_by_shape(&elsewhere.path().join(MAKI_DIR).join(name))
    }

    /// `permissions.toml` is the whole truth about what it opened, at every
    /// moment and not only at the first load. A line the user deleted before
    /// `/reload` has to close the path again: appending each load instead left
    /// the guard holding a grant no file on disk described, which only a
    /// restart would have taken back and which nothing in the config explained.
    ///
    /// A live answer is the other half and is not a line in any file, so a
    /// reload of the config must not drop it either.
    #[test]
    fn a_reload_holds_exactly_what_the_config_says_now() {
        let roles = Roles::new();
        let guard = roles.guard();
        let by_config = roles.logs.join(NOTE_FILE);
        let by_answer = roles.state.join(NOTE_FILE);

        guard.install_config([guard.stage(&by_config, Access::Read).unwrap()]);
        guard.add_override(&by_answer, Access::Read).unwrap();
        assert!(!guard.is_unreachable(&by_config, Access::Read));

        guard.install_config([]);

        assert!(
            guard.is_unreachable(&by_config, Access::Read),
            "the guard has to hold what permissions.toml says now, not what it once said"
        );
        assert!(
            !guard.is_unreachable(&by_answer, Access::Read),
            "a live answer is in no file, so rereading the files cannot take it back"
        );
    }

    /// An answer over a path an earlier answer already opened still stages, so
    /// the "always" a user gives after a "once" over the same file is still
    /// written down where the next start reads it. `override_candidate` goes
    /// quiet the moment a path is open, which is the right answer to "does this
    /// call need a prompt" and the wrong one to "is there anything to record".
    #[test]
    fn an_answer_over_an_already_open_path_still_stages() {
        let roles = Roles::new();
        let guard = roles.guard();
        let path = roles.state.join(NOTE_FILE);

        guard.add_override(&path, Access::Read).unwrap();

        assert_eq!(guard.override_candidate(&path, Access::Read), None);
        let staged = guard.stage(&path, Access::Read).expect(
            "an answer is worth recording even where an earlier one already opened the path",
        );
        assert_eq!(
            staged.path(),
            canonical_key(&path),
            "a durable grant names the file the guard matched, never the spelling that reached it"
        );
    }

    /// The widest override there is, put into the list behind the back of the
    /// constructor that would have refused it. What a caller cannot do, so that
    /// a rule can be shown to hold even if one ever could.
    fn inject_override(guard: &Guard, path: &Path) {
        guard.overrides.store(Arc::new(Overrides {
            config: vec![(canonical_key(path), Access::Write)],
            granted: Vec::new(),
        }));
    }

    /// Strictly stronger than "the longest prefix wins": the override goes
    /// straight into the list, past the constructor that would have refused it,
    /// and the path still refuses. The branch that reads overrides does not run
    /// for these rules, so no entry can reach them however it was spelled.
    #[test_case(|state| state.join(crate::auth::AUTH_DIR).join(NOTE_FILE); "under_the_credentials")]
    #[test_case(|state| state.join(APPROVALS_FILE); "the_approval_store")]
    fn an_injected_override_does_not_open_a_sealed_path(spell: fn(&Path) -> PathBuf) {
        let roles = Roles::new();
        let guard = roles.guard();
        let path = spell(&roles.state);
        inject_override(&guard, &path);

        for access in Access::ALL {
            assert!(
                guard.is_unreachable(&path, access),
                "no override reaches a rule that allows none"
            );
        }
    }

    /// A guard that cannot tell where its own files live must not honour a path
    /// claiming to be inside one, whether the claim came from the config or from
    /// an approval. A walk gets the same treatment: it has to keep asking, or it
    /// would prune its way past rules the guard cannot name.
    #[test]
    fn a_degraded_guard_honours_no_override() {
        let elsewhere = tempfile::TempDir::new().unwrap();
        let guard = guard_from(None, None, &[], None);
        let path = elsewhere.path().join(NOTE_FILE);

        assert_eq!(
            guard.override_candidate(&path, Access::Read),
            None,
            "{FAIL_CLOSED}"
        );
        assert!(
            matches!(
                guard.add_override(&path, Access::Read),
                Err(OverrideError::Degraded { .. })
            ),
            "{FAIL_CLOSED}"
        );
        inject_override(&guard, &path);
        assert!(guard.is_unreachable(&path, Access::Read), "{FAIL_CLOSED}");
        assert!(
            guard.may_cover_subtree(elsewhere.path()),
            "a guard with no rules cannot let a walk stop asking"
        );
    }

    /// An override is for Maki's own files. A path no rule covers is reachable
    /// already, so an entry naming one is a mistake worth reporting rather than
    /// a line that grants nothing.
    #[test]
    fn an_ordinary_path_is_not_an_override() {
        let roles = Roles::new();
        let elsewhere = tempfile::TempDir::new().unwrap();

        assert!(matches!(
            roles
                .guard()
                .add_override(&elsewhere.path().join(NOTE_FILE), Access::Read),
            Err(OverrideError::Unnecessary { .. })
        ));
    }

    /// A walk has to descend into a subtree an approval opened, for the same
    /// reason it descends into one an open rule opens: pruning at the closed
    /// parent is what hid the memory notes.
    #[test]
    fn a_walk_descends_into_an_override() {
        let roles = Roles::new();
        let guard = roles.guard();
        let opened = roles.logs.join(SUBDIR);

        assert!(
            guard.may_skip_key(&canonical_key(&roles.logs)),
            "nothing under the logs is readable yet"
        );
        guard
            .add_override(&opened, Access::Read)
            .expect(OVERRIDE_IS_LEGAL);
        assert!(
            !guard.may_skip_key(&canonical_key(&roles.logs)),
            "{PRUNE_HIDES_THE_OVERRIDE}"
        );
        assert!(
            !guard.may_skip_key(&canonical_key(&opened)),
            "{PRUNE_HIDES_THE_OVERRIDE}"
        );
    }

    /// A removal takes what is under it, so the question it asks is the write
    /// question, and the answer an approval gives to the read one does not
    /// carry: the whole point of [`Reach::ReadByApproval`] is that a user can
    /// hand over a log to read and still not lose it to an `rm -r`.
    #[test]
    fn a_removal_survives_a_read_approval() {
        let roles = Roles::new();
        let guard = roles.guard();

        assert!(
            matches!(
                guard.add_override(&roles.logs, Access::Write),
                Err(OverrideError::NeverOverridable { .. })
            ),
            "Maki trusts what it keeps here on its next start, so no answer makes it writable"
        );
        guard
            .add_override(&roles.logs, Access::Read)
            .expect(OVERRIDE_IS_LEGAL);
        assert!(
            guard.contains_unwritable(&roles.logs),
            "a read is all that was handed over, and what is under it is nobody's to remove"
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
        let guard = guard_from(Some(state.path()), None, &[], None);

        let spelled = spell(&link);
        assert!(
            guard.is_unreachable(&spelled, Access::Read),
            "a lexical spelling must not decide the rule"
        );
        assert!(
            !guard.is_unreachable_key(&normalize_path(&spelled), Access::Read),
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
        // SAFETY: setting a variable is only sound while no other thread reads
        // the environment, and the runner is what holds that up: `just test`
        // runs `cargo nextest`, which gives every test its own process.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", hostile.path()) };

        let dirs = config_search_dirs_from(Some(home_a.path()), Some(&xdg_a));

        // SAFETY: same one process per test rule as above.
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
}
