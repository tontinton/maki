use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use maki_config::{host_allowed, is_valid_host_pattern};
use maki_providers::plugin;
use mlua::{Error as LuaError, Function, IntoLuaMulti, Lua, Result as LuaResult};
use semver::Version;
use tracing::warn;
use url::{Origin, Url};

use crate::error::PluginError;

pub use maki_config::Permission;

pub(crate) const MANIFEST_FILE: &str = "plugin.toml";
pub(crate) const NET_HOSTS_KEY: &str = "net_hosts";
const PROVIDERS_KEY: &str = "providers";
const MIN_MAKI_VERSION: &str = "min_maki_version";
const PERMISSION_DENIED: &str = "permission denied";
pub(crate) const OUTSIDE_CEILING: &str =
    "is not in the capability of the action this code runs for";
const PROVIDER_ORIGIN_SHORTFALL: &str =
    "reaches a provider address only its own plugin, or one granted 'run', may use";
const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The hosts a plugin declared it talks to, or `None` when it declared no
/// list at all.
///
/// The two are different answers, not a list and an empty one: `net = true`
/// with no `net_hosts` predates the allowlist and keeps meaning "any public
/// host", while a list present means exactly those hosts. Shared because every
/// `maki.net` call in one plugin reads the same list.
pub type NetHosts = Option<Arc<[String]>>;

/// The slugs one plugin registered a provider for, captured when its `maki`
/// global was built. Nothing on the Lua side names a plugin, so a plugin
/// reaches its own providers and no one else's by construction rather than by
/// a check it could be handed the wrong argument for.
pub type OwnedSlugs = Arc<Mutex<Vec<String>>>;

/// Where a plugin is allowed to send bytes, as one answer rather than two
/// lists that a call site could consult one of.
///
/// A manifest can only name the hosts its author knew about. The origin of a
/// provider *this plugin registered* is the other half, and only maki knows
/// it: the user repoints a slug with `<SLUG>_BASE_URL` or `providers.toml`,
/// and no `plugin.toml` written beforehand can have that host in it. Leaving
/// it out made a provider's own `fetch_usage` unreachable for exactly the
/// users who need it, while the codec went to that very origin with the very
/// same credentials and was never questioned.
#[derive(Clone, Debug, Default)]
pub struct NetEgress {
    declared: NetHosts,
    providers: OwnedSlugs,
}

impl NetEgress {
    pub fn new(declared: NetHosts) -> Self {
        Self {
            declared,
            providers: OwnedSlugs::default(),
        }
    }

    /// The raw manifest list. `allows` reads a missing list as "any host",
    /// and some callers need to treat it as "no host" instead.
    pub(crate) fn declared(&self) -> &NetHosts {
        &self.declared
    }

    /// Records a slug [`crate::api::provider`] just registered. Kept here and
    /// not beside the registration so the network layer cannot be given a
    /// stale copy: both surfaces read this one cell.
    pub(crate) fn owns(&self, slug: String) {
        self.providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(slug);
    }

    pub(crate) fn owned(&self) -> OwnedSlugs {
        Arc::clone(&self.providers)
    }

    /// Whether this plugin may reach `host:port`: its manifest says so, or it
    /// is where maki itself would send the credentials of a provider the
    /// plugin registered.
    pub(crate) fn allows(&self, host: &str, port: u16) -> bool {
        let Some(declared) = &self.declared else {
            return true;
        };
        host_allowed(host, port, declared) || self.serves(host, port)
    }

    /// Whether `url` is on the origin of a provider this plugin registered,
    /// as chosen by the user or by maki. See [`plugin::vouched_origin`].
    pub(crate) fn vouches(&self, url: &Url) -> bool {
        self.vouched_origins().contains(&url.origin())
    }

    /// Collected instead of checked under the lock. A command check can compare
    /// a plugin with itself, and then both sides take this same lock.
    fn vouched_origins(&self) -> Vec<Origin> {
        self.providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|slug| plugin::vouched_origin(slug))
            .map(|url| url.origin())
            .collect()
    }

    /// What {other} reaches that this egress does not. Hosts compare as plain
    /// text, so `*.example.com` does not cover `api.example.com`. That can only
    /// refuse too much.
    ///
    /// A provider origin skips the private-address guard, so no `net_hosts`
    /// entry can stand in for it. Only a provider on the same origin can.
    fn uncovered(&self, other: &Self) -> Option<Missing> {
        let hosts_covered = match (&self.declared, &other.declared) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(mine), Some(theirs)) => theirs.iter().all(|host| mine.contains(host)),
        };
        if !hosts_covered {
            return Some(Missing::Hosts);
        }
        let mine = self.vouched_origins();
        let origins_covered = other
            .vouched_origins()
            .iter()
            .all(|origin| mine.contains(origin));
        (!origins_covered).then_some(Missing::ProviderOrigin)
    }

    fn serves(&self, host: &str, port: u16) -> bool {
        self.providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|slug| plugin::effective_authority(slug))
            .any(|origin| host_allowed(host, port, &[origin]))
    }
}

#[derive(Debug, Clone)]
pub struct PluginPermissions {
    allowed: [bool; Permission::COUNT],
    net_hosts: NetHosts,
    /// The plugin whose `maki` global holds these grants, which a
    /// [`Ceiling`] is matched against. `None` for grants no global holds yet.
    owner: Option<Arc<str>>,
}

impl PluginPermissions {
    pub fn trusted() -> Self {
        Self {
            allowed: [true; Permission::COUNT],
            net_hosts: None,
            owner: None,
        }
    }

    pub fn denied() -> Self {
        Self {
            allowed: [false; Permission::COUNT],
            net_hosts: None,
            owner: None,
        }
    }

    /// These grants as {plugin}'s, so the [`Ceiling`] of an action {plugin}
    /// owns applies to them.
    pub(crate) fn owned_by(mut self, plugin: Arc<str>) -> Self {
        self.owner = Some(plugin);
        self
    }

    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    pub fn net_hosts(&self) -> NetHosts {
        self.net_hosts.clone()
    }

    pub fn set_net_hosts(&mut self, hosts: NetHosts) {
        self.net_hosts = hosts;
    }

    /// Builds a set from the names an approval records.
    ///
    /// A name this build does not know is ignored rather than treated as a
    /// grant, so an approval file written by a newer maki cannot widen what an
    /// older one allows.
    pub fn from_approved<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut out = Self::denied();
        for name in names {
            if let Some(perm) = Permission::from_key(name) {
                out.set(perm, true);
            }
        }
        out
    }

    pub fn is_allowed(&self, perm: Permission) -> bool {
        self.allowed[perm as usize]
    }

    pub fn lacking(&self) -> impl Iterator<Item = Permission> + '_ {
        Permission::ALL
            .iter()
            .copied()
            .filter(|&perm| !self.is_allowed(perm))
    }

    /// Reaching what nobody declared costs every capability a tool can reach,
    /// since no narrower price would be honest. `agent` is not one of them:
    /// it is about starting turns, and every tool already runs inside one, so
    /// a plugin that may not start turns can still steer a call in one.
    /// `input` is not either, since no built-in tool changes the chat input.
    pub fn holds_all_reach(&self) -> bool {
        !self.lacking().any(Self::is_reach)
    }

    pub(crate) fn is_reach(perm: Permission) -> bool {
        !matches!(perm, Permission::Agent | Permission::Input)
    }

    pub fn from_manifest(manifest: &toml::Value) -> Self {
        let perms = manifest.get("permissions");
        let mut allowed = [true; Permission::COUNT];
        for &perm in Permission::ALL {
            allowed[perm as usize] = perms
                .and_then(|p| p.get(perm.manifest_key()))
                .and_then(toml::Value::as_bool)
                .unwrap_or(true);
        }
        Self {
            allowed,
            net_hosts: net_hosts_from_manifest(manifest),
            owner: None,
        }
    }

    pub fn set(&mut self, perm: Permission, value: bool) {
        self.allowed[perm as usize] = value;
    }

    /// The first of {needed} this set lacks, in the order the guard lists them.
    pub(crate) fn first_denied(&self, needed: &[Permission]) -> Option<Permission> {
        needed.iter().copied().find(|&perm| !self.is_allowed(perm))
    }

    /// The grants both this and {other} hold, with this set's hosts and owner.
    pub(crate) fn intersect(&self, other: &Self) -> Self {
        let mut out = self.clone();
        for &perm in Permission::ALL {
            out.set(perm, self.is_allowed(perm) && other.is_allowed(perm));
        }
        out
    }

    /// This grant as far as the code running now may use it, see [`Ceiling`].
    pub(crate) fn now(&self, lua: &Lua) -> Self {
        match lua.app_data_ref::<Ceiling>() {
            Some(ceiling) if ceiling.binds(self.owner()) => self.intersect(&ceiling.0),
            _ => self.clone(),
        }
    }

    /// A function that needs every permission in {needed}. Without one of
    /// them it is replaced by a stub naming the first one missing, and with
    /// them all it still asks the [`Ceiling`] at every call.
    pub fn guard<F, A, R>(&self, needed: &[Permission], lua: &Lua, f: F) -> LuaResult<Function>
    where
        F: Fn(&Lua, A) -> LuaResult<R> + Send + 'static,
        A: mlua::FromLuaMulti,
        R: IntoLuaMulti,
    {
        match self.first_denied(needed) {
            None => {
                let owner = self.owner.clone();
                let needed: Box<[Permission]> = needed.into();
                lua.create_function(move |lua, args| {
                    check_ceiling(lua, owner.as_deref(), &needed)?;
                    f(lua, args)
                })
            }
            Some(perm) => denied_stub(lua, perm),
        }
    }

    pub fn guard_async<F, Fut, A, R>(
        &self,
        needed: &[Permission],
        lua: &Lua,
        f: F,
    ) -> LuaResult<Function>
    where
        F: Fn(Lua, A) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = LuaResult<R>> + Send + 'static,
        A: mlua::FromLuaMulti,
        R: IntoLuaMulti,
    {
        match self.first_denied(needed) {
            None => {
                let owner = self.owner.clone();
                let needed: Box<[Permission]> = needed.into();
                lua.create_async_function(move |lua, args| {
                    let call =
                        check_ceiling(&lua, owner.as_deref(), &needed).map(|()| f(lua, args));
                    async move { call?.await }
                })
            }
            Some(perm) => denied_stub(lua, perm),
        }
    }
}

/// A plugin's grant plus the egress `maki.net` checks, provider origins and all.
///
/// Build it once per `maki` global and clone it from there. The egress learns
/// about each provider as the plugin registers it, so a command has to carry
/// this very value. A fresh one would know none of them and let through the
/// borrowing it is meant to catch.
#[derive(Clone, Debug)]
pub struct PluginReach {
    permissions: PluginPermissions,
    egress: NetEgress,
}

impl PluginReach {
    pub fn new(permissions: PluginPermissions) -> Self {
        let egress = NetEgress::new(permissions.net_hosts());
        Self {
            permissions,
            egress,
        }
    }

    pub fn is_allowed(&self, perm: Permission) -> bool {
        self.permissions.is_allowed(perm)
    }

    /// The plugin this reach is, `None` for one that no `maki` global holds.
    pub fn owner(&self) -> Option<&Arc<str>> {
        self.permissions.owner.as_ref()
    }

    /// See [`PluginPermissions::owned_by`].
    pub fn owned_by(&self, plugin: Arc<str>) -> Self {
        Self {
            permissions: self.permissions.clone().owned_by(plugin),
            egress: self.egress.clone(),
        }
    }

    pub(crate) fn permissions(&self) -> &PluginPermissions {
        &self.permissions
    }

    pub(crate) fn egress(&self) -> &NetEgress {
        &self.egress
    }

    /// This reach as far as the code running now may use it, see [`Ceiling`].
    pub(crate) fn now(&self, lua: &Lua) -> Self {
        Self {
            permissions: self.permissions.now(lua),
            egress: self.egress.clone(),
        }
    }

    /// The grants both this and {other} hold. The egress and owner are this
    /// reach's, so {other} has to be the same plugin's reach, which shares
    /// that egress.
    pub fn intersect(&self, other: &Self) -> Self {
        Self {
            permissions: self.permissions.intersect(&other.permissions),
            egress: self.egress.clone(),
        }
    }

    /// This reach cut down to {perms}, the part of it an owner says a caller
    /// has to match. The egress stays, so a `net` among them is still
    /// weighed host by host.
    pub(crate) fn only(&self, perms: &[Permission]) -> Self {
        let mut permissions = self.permissions.clone();
        for &perm in Permission::ALL {
            permissions.set(perm, perms.contains(&perm));
        }
        Self {
            permissions,
            egress: self.egress.clone(),
        }
    }

    /// Asked when one plugin runs another's command. The handler runs with
    /// {needed}, so a caller that falls short anywhere could borrow that reach
    /// and pick the arguments.
    ///
    /// A caller with `run` skips the egress check. Any process it starts can
    /// reach any host anyway, so comparing addresses would only refuse.
    pub fn first_missing(&self, needed: &Self) -> Option<Missing> {
        if let Some(perm) = Permission::ALL
            .iter()
            .copied()
            .find(|&perm| needed.is_allowed(perm) && !self.is_allowed(perm))
        {
            return Some(Missing::Permission(perm));
        }
        if !needed.is_allowed(Permission::Net) || self.is_allowed(Permission::Run) {
            return None;
        }
        self.egress.uncovered(&needed.egress)
    }
}

/// Why a plugin may not run something, one variant per fix the author can
/// make, so the message never points at the wrong one.
#[derive(Debug, PartialEq, Eq)]
pub enum Missing {
    Permission(Permission),
    Hosts,
    ProviderOrigin,
}

impl Missing {
    /// What the plugin lacks, after the name of what it tried to run.
    fn shortfall(&self) -> String {
        match self {
            Self::Permission(perm) => format!("needs '{perm}'"),
            Self::Hosts => format!("reaches hosts this plugin's {NET_HOSTS_KEY} leaves out"),
            Self::ProviderOrigin => PROVIDER_ORIGIN_SHORTFALL.to_owned(),
        }
    }

    fn fix(&self) -> Option<String> {
        let fix = match self {
            Self::Permission(_) => "grant it",
            Self::Hosts => "list them",
            Self::ProviderOrigin => return None,
        };
        Some(format!("{fix} in {MANIFEST_FILE} next to the plugin file"))
    }

    /// The refusal for {name}, short enough to flash: the name, what is
    /// missing and the fix come first, so a line cut to the screen's width
    /// still says all three.
    pub fn refusal(&self, name: &str) -> String {
        let shortfall = self.shortfall();
        match self.fix() {
            Some(fix) => format!("{name} {shortfall}: {fix}"),
            None => format!("{name} {shortfall}"),
        }
    }
}

/// The one edit that closes a permission gap. Every key is named because a
/// user `init.lua` with no manifest is denied everything, and a bare "denied"
/// does not tell its author that a file is missing, let alone which one.
pub(crate) fn grant_hint(perms: &[Permission]) -> String {
    let keys = perms
        .iter()
        .map(|perm| format!("`{perm} = true`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("Add {keys} under `[permissions]` in the {MANIFEST_FILE} next to the plugin file")
}

/// What a package's `plugin.toml` asks for.
///
/// Deny by default: an omitted key is *not requested*. That is the opposite of
/// [`PluginPermissions::from_manifest`], which stays permissive for a local
/// `init.lua` the user wrote themselves. The two parsers are separate because
/// the inputs differ in trust, not in shape: a package manifest arrives with
/// downloaded code and must not be able to widen its own access.
///
/// This is a distinct type so an effective grant can never be built by
/// accident from a request alone.
#[derive(Debug, Clone)]
pub struct Requested {
    permissions: PluginPermissions,
    /// Catalog provider names the package takes over, as its manifest lists
    /// them. Shown in the approval prompt so the user sees the package will
    /// receive the key saved for each name.
    providers: Vec<String>,
}

impl Requested {
    pub fn none() -> Self {
        Self {
            permissions: PluginPermissions::denied(),
            providers: Vec::new(),
        }
    }

    pub fn from_manifest(manifest: &toml::Value) -> Self {
        let perms = manifest.get("permissions");
        let mut allowed = [false; Permission::COUNT];
        for &perm in Permission::ALL {
            allowed[perm as usize] = perms
                .and_then(|p| p.get(perm.manifest_key()))
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
        }
        Self {
            permissions: PluginPermissions {
                allowed,
                net_hosts: net_hosts_from_manifest(manifest),
                owner: None,
            },
            providers: providers_from_manifest(manifest),
        }
    }

    pub fn is_requested(&self, perm: Permission) -> bool {
        self.permissions.is_allowed(perm)
    }

    /// The hosts the manifest asks to reach, `None` for every host. A request,
    /// like every other name in here, so the approval store is what decides
    /// whether they are granted.
    pub fn net_hosts(&self) -> Option<&[String]> {
        self.permissions.net_hosts.as_deref()
    }

    /// Every slug the manifest lists as registered by the package.
    pub fn providers(&self) -> &[String] {
        &self.providers
    }

    /// Slugs the request and the approval both name: what a load may
    /// register. A manifest alone is a request, like every other name in
    /// here, so a slug the approval never covered stays out.
    pub fn granted_providers(&self, approved: &[String]) -> Vec<String> {
        self.providers
            .iter()
            .filter(|slug| approved.contains(slug))
            .cloned()
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        Permission::ALL
            .iter()
            .filter(|permission| self.is_requested(**permission))
            .map(|permission| permission.to_string())
            .collect()
    }

    /// Code whose files nobody fetched gets what it asks for: a package the
    /// user installed by hand, or a plugin bundled into the binary. Only a
    /// package maki downloaded has to be intersected with an approval.
    pub fn granted(self) -> PluginPermissions {
        self.permissions
    }

    /// Effective permissions for a managed package: the request and the user's
    /// approval must agree.
    ///
    /// The host list comes from the request alone. It is a narrowing the
    /// package wrote about itself, and the approval it needs is the decision to
    /// load the package at all, which the caller has already made by the time
    /// the two are intersected.
    pub fn intersect(&self, approved: &PluginPermissions) -> PluginPermissions {
        let mut out = PluginPermissions::denied();
        for &perm in Permission::ALL {
            out.set(
                perm,
                self.permissions.is_allowed(perm) && approved.is_allowed(perm),
            );
        }
        out.set_net_hosts(self.permissions.net_hosts());
        out
    }
}

/// `providers = ["greenpt"]` at the top level of `plugin.toml`.
///
/// Entries that are not strings are dropped, since a manifest cannot be
/// trusted to be well formed and a malformed entry must not widen the list
/// it appears in.
fn providers_from_manifest(manifest: &toml::Value) -> Vec<String> {
    manifest
        .get(PROVIDERS_KEY)
        .and_then(toml::Value::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|slug| slug.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// `[permissions] net_hosts = ["api.example.com", "*.example.com"]`.
///
/// An absent key is `None` and not an empty list: see [`NetHosts`]. Entries
/// that are not strings are dropped, since a manifest cannot be trusted to be
/// well formed and a malformed entry must not widen the list it appears in.
fn net_hosts_from_manifest(manifest: &toml::Value) -> NetHosts {
    let hosts = manifest
        .get("permissions")?
        .get(NET_HOSTS_KEY)?
        .as_array()?
        .iter()
        .filter_map(|host| host.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    Some(hosts.into())
}

/// A typo in `net_hosts` grants nothing. Without this warning the author only
/// sees a blocked request later, with no hint that the list itself was wrong.
pub(crate) fn warn_invalid_net_hosts(plugin: &str, hosts: &NetHosts) {
    let entries = hosts.as_deref().unwrap_or_default();
    for entry in entries.iter().filter(|entry| !is_valid_host_pattern(entry)) {
        warn!(plugin, entry, "ignoring unparseable net_hosts entry");
    }
}

/// Reads a package's requested permissions.
///
/// Only an absent manifest means "requests nothing". A manifest that exists but
/// cannot be read or parsed is an error, because silently treating it as empty
/// would load the package and then fail every guarded call it makes, which
/// reports the typo as a permission problem instead of a syntax one.
pub(crate) fn load_requested_permissions(
    plugin_dir: &Path,
) -> Result<Requested, crate::error::PluginError> {
    let manifest_path = plugin_dir.join(MANIFEST_FILE);
    let content = match std::fs::read_to_string(&manifest_path) {
        Ok(content) => content,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Requested::none()),
        Err(source) => {
            return Err(crate::error::PluginError::Io {
                path: manifest_path,
                source,
            });
        }
    };
    requested_permissions_from_text(&content, &manifest_path)
}

pub(crate) fn requested_permissions_from_text(
    content: &str,
    manifest_path: &Path,
) -> Result<Requested, PluginError> {
    toml::from_str::<toml::Value>(content)
        .map(|value| Requested::from_manifest(&value))
        .map_err(|error| PluginError::PackageManifest {
            path: manifest_path.to_path_buf(),
            message: error.to_string(),
        })
}

fn denied_stub(lua: &Lua, perm: Permission) -> LuaResult<Function> {
    lua.create_function(move |_, _: mlua::MultiValue| -> LuaResult<mlua::Value> {
        Err(denied_error(perm))
    })
}

/// The most the code of one plugin may use while it runs, kept as Lua app
/// data.
///
/// A plugin action runs for its caller with only its price, the owner's
/// grants cut down to the capability it declared. The caller set up what the
/// callback reads, like the chat input, so with the owner's other grants it
/// could steer them. Every guarded call of the owner checks this, and so does
/// the reach a call hands to the UI. The tasks and timers the owner starts
/// meanwhile inherit it. Other plugins' code that runs inside, like an
/// autocmd the callback fires, keeps its own grants: the owner's grants
/// were never theirs to lose.
#[derive(Clone, Debug)]
pub(crate) struct Ceiling(pub(crate) PluginPermissions);

impl Ceiling {
    /// The ceiling up now when it binds {plugin}, which a task {plugin}
    /// starts runs under.
    pub(crate) fn current_for(lua: &Lua, plugin: &str) -> Option<Self> {
        lua.app_data_ref::<Self>()
            .filter(|ceiling| ceiling.binds(Some(plugin)))
            .map(|ceiling| ceiling.clone())
    }

    /// Grants no `maki` global holds belong to no plugin, so no ceiling is
    /// theirs, not even one that lost its owner by mistake.
    fn binds(&self, owner: Option<&str>) -> bool {
        owner.is_some() && self.0.owner() == owner
    }
}

fn check_ceiling(lua: &Lua, owner: Option<&str>, needed: &[Permission]) -> LuaResult<()> {
    match lua.app_data_ref::<Ceiling>() {
        Some(ceiling) if ceiling.binds(owner) => match ceiling.0.first_denied(needed) {
            Some(perm) => Err(ceiling_error(perm)),
            None => Ok(()),
        },
        _ => Ok(()),
    }
}

/// `Err` when {granted}, checked outside [`PluginPermissions::guard`], does
/// not hold {perm} for the code running now.
pub(crate) fn require(lua: &Lua, granted: &PluginPermissions, perm: Permission) -> LuaResult<()> {
    if !granted.is_allowed(perm) {
        return Err(denied_error(perm));
    }
    check_ceiling(lua, granted.owner(), &[perm])
}

fn ceiling_error(perm: Permission) -> LuaError {
    let msg = format!("{PERMISSION_DENIED}: '{perm}' {OUTSIDE_CEILING}");
    warn!(permission = %perm, "{msg}");
    LuaError::runtime(msg)
}

pub(crate) fn denied_error(perm: Permission) -> LuaError {
    let msg = format!(
        "{PERMISSION_DENIED}: '{perm}' not granted for this plugin. {}",
        grant_hint(&[perm])
    );
    warn!(permission = %perm, "{msg}");
    LuaError::runtime(msg)
}

pub(crate) fn load_plugin_permissions(plugin_dir: Option<&Path>) -> PluginPermissions {
    load_plugin_manifest(plugin_dir)
        .as_ref()
        .map_or_else(PluginPermissions::denied, PluginPermissions::from_manifest)
}

/// The `providers` list a `plugin.toml` asks for. A plugin loaded straight
/// from disk has no approval store behind it, so its own manifest is the grant.
pub(crate) fn load_plugin_granted_providers(plugin_dir: Option<&Path>) -> Vec<String> {
    load_plugin_manifest(plugin_dir)
        .as_ref()
        .map(|manifest| Requested::from_manifest(manifest).providers().to_vec())
        .unwrap_or_default()
}

/// Host-side gate, run before any Lua from `plugin_dir` reaches the runtime.
/// An `Err` means the directory is refused; the startup path turns it into a
/// warning and skips the plugin, so one bad `min_maki_version` cannot keep
/// Maki from booting.
pub(crate) fn check_plugin_compatibility(
    plugin: &str,
    plugin_dir: Option<&Path>,
) -> Result<(), PluginError> {
    let Some(manifest) = load_plugin_manifest(plugin_dir) else {
        return Ok(());
    };
    let Some(required) = manifest.get(MIN_MAKI_VERSION) else {
        return Ok(());
    };
    check_minimum_version(plugin, required, RUNTIME_VERSION)
}

fn load_plugin_manifest(plugin_dir: Option<&Path>) -> Option<toml::Value> {
    let dir = plugin_dir?;
    let manifest_path = dir.join(MANIFEST_FILE);
    match std::fs::read_to_string(&manifest_path) {
        Ok(content) => match toml::from_str::<toml::Value>(&content) {
            Ok(manifest) => Some(manifest),
            Err(e) => {
                warn!(
                    path = %manifest_path.display(),
                    error = %e,
                    "invalid {MANIFEST_FILE}, denying all permissions"
                );
                None
            }
        },
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                warn!(
                    dir = %dir.display(),
                    "no {MANIFEST_FILE} next to plugin; all permissions denied. Create one \
                     (even an empty file) next to it to grant permissions"
                );
            } else {
                warn!(
                    path = %manifest_path.display(),
                    error = %e,
                    "cannot read {MANIFEST_FILE}, denying all permissions"
                );
            }
            None
        }
    }
}

fn check_minimum_version(
    plugin: &str,
    required: &toml::Value,
    running: &str,
) -> Result<(), PluginError> {
    let required = required
        .as_str()
        .ok_or_else(|| PluginError::InvalidMinimumVersionType {
            plugin: plugin.to_owned(),
        })?;
    let required =
        Version::parse(required).map_err(|source| PluginError::InvalidMinimumVersion {
            plugin: plugin.to_owned(),
            version: required.to_owned(),
            source,
        })?;
    let running = Version::parse(running).map_err(|source| PluginError::InvalidRuntimeVersion {
        version: running.to_owned(),
        source,
    })?;
    if required > running {
        return Err(PluginError::MakiVersionTooOld {
            plugin: plugin.to_owned(),
            required,
            running,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use maki_providers::test_support::register_bundled;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const PLUGIN: &str = "test-plugin";
    const DECLARED_HOST: &str = "api.example.com";
    const OTHER_HOST: &str = "elsewhere.example";
    const HTTPS_PORT: u16 = 443;
    const UNREGISTERED_SLUG: &str = "not-a-provider";
    const LOCAL_PROVIDER_SLUG: &str = "lan-box";
    const LOCAL_PROVIDER_HOST: &str = "127.0.0.1:11434";
    const LOCAL_PROVIDER_URL: &str = "http://127.0.0.1:11434/v1";

    fn assert_denied(permissions: &PluginPermissions) {
        for &permission in Permission::ALL {
            assert!(
                !permissions.is_allowed(permission),
                "{permission} should be denied"
            );
        }
    }

    /// The grant a registered provider adds is covered end to end by
    /// `tests/provider_replay.rs`, where a real slug has a real origin.
    /// What is worth pinning here is the other direction: owning a slug is
    /// never a grant by itself, so a plugin cannot widen its own reach by
    /// naming providers that do not resolve.
    #[test_case(None, &[], OTHER_HOST, true ; "a_plugin_that_declared_nothing_is_unrestricted")]
    #[test_case(Some(DECLARED_HOST), &[], DECLARED_HOST, true ; "a_declared_host_is_reachable")]
    #[test_case(Some(DECLARED_HOST), &[], OTHER_HOST, false ; "and_nothing_else_is")]
    #[test_case(Some(DECLARED_HOST), &[UNREGISTERED_SLUG], OTHER_HOST, false ; "owning_a_slug_that_serves_nothing_grants_nothing")]
    fn egress_reaches(declared: Option<&str>, owns: &[&str], host: &str, expected: bool) {
        let egress = NetEgress::new(declared.map(|host| Arc::from(vec![host.to_owned()])));
        for slug in owns {
            egress.owns((*slug).to_owned());
        }
        assert_eq!(egress.allows(host, HTTPS_PORT), expected);
    }

    #[test]
    fn trusted_allows_everything() {
        let p = PluginPermissions::trusted();
        for &perm in Permission::ALL {
            assert!(p.is_allowed(perm), "{perm} should be allowed");
        }
    }

    #[test]
    fn denied_blocks_everything() {
        let p = PluginPermissions::denied();
        assert_denied(&p);
    }

    #[test]
    fn full_reach_needs_neither_agent_nor_input_but_misses_any_capability() {
        let mut p = PluginPermissions::trusted();
        p.set(Permission::Agent, false);
        p.set(Permission::Input, false);
        assert!(p.holds_all_reach());

        p.set(Permission::Env, false);
        assert!(!p.holds_all_reach());
    }

    /// Everything but `run`, which would skip the egress comparison.
    fn with_hosts(hosts: Option<&[&str]>) -> PluginReach {
        let mut p = PluginPermissions::trusted();
        p.set(Permission::Run, false);
        p.set_net_hosts(hosts.map(|hosts| hosts.iter().map(|h| (*h).to_owned()).collect()));
        PluginReach::new(p)
    }

    fn with_hosts_and_run(hosts: Option<&[&str]>) -> PluginReach {
        let mut p = with_hosts(hosts).permissions;
        p.set(Permission::Run, true);
        PluginReach::new(p)
    }

    #[test_case(with_hosts(None), with_hosts(None), None ; "any_host_covers_any_host")]
    #[test_case(with_hosts(None), with_hosts(Some(&[DECLARED_HOST])), None ; "any_host_covers_a_list")]
    #[test_case(with_hosts(Some(&[DECLARED_HOST])), with_hosts(None), Some(Missing::Hosts) ; "a_list_does_not_cover_any_host")]
    #[test_case(with_hosts(Some(&[DECLARED_HOST])), with_hosts(Some(&[DECLARED_HOST])), None ; "a_list_covers_itself")]
    #[test_case(with_hosts(Some(&[DECLARED_HOST])), with_hosts(Some(&[OTHER_HOST])), Some(Missing::Hosts) ; "a_list_does_not_cover_another_host")]
    #[test_case(with_hosts_and_run(Some(&[DECLARED_HOST])), with_hosts(Some(&[OTHER_HOST])), None ; "a_caller_that_runs_processes_skips_hosts")]
    fn first_missing_compares_net_hosts(
        caller: PluginReach,
        needed: PluginReach,
        expected: Option<Missing>,
    ) {
        assert_eq!(caller.first_missing(&needed), expected);
    }

    /// A bundled provider's address skips the private-address guard, so even
    /// a plugin allowed every public host falls short of it. The owner checking
    /// itself also proves the provider list is not locked twice.
    #[test_case(|_| with_hosts(None), Some(Missing::ProviderOrigin) ; "another_plugin_falls_short")]
    #[test_case(|_| with_hosts_and_run(None), None ; "a_caller_that_runs_processes_skips_origins")]
    #[test_case(PluginReach::clone, None ; "the_owner_covers_itself")]
    fn first_missing_compares_provider_origins(
        caller: fn(&PluginReach) -> PluginReach,
        expected: Option<Missing>,
    ) {
        register_bundled(
            json!({
                "slug": LOCAL_PROVIDER_SLUG,
                "display_name": LOCAL_PROVIDER_SLUG,
                "codec": "openai",
                "base_url": LOCAL_PROVIDER_URL,
                "models": [{ "prefixes": [LOCAL_PROVIDER_SLUG], "tier": "strong" }],
            }),
            LOCAL_PROVIDER_HOST,
        );
        let owner = with_hosts(Some(&[LOCAL_PROVIDER_HOST]));
        owner.egress().owns(LOCAL_PROVIDER_SLUG.to_owned());

        assert_eq!(caller(&owner).first_missing(&owner), expected);
    }

    #[test]
    fn from_manifest_partial() {
        let val: toml::Value = toml::from_str(
            r#"
            [permissions]
            fs_read = false
            net = false
            "#,
        )
        .unwrap();
        let p = PluginPermissions::from_manifest(&val);
        assert!(!p.is_allowed(Permission::FsRead));
        assert!(p.is_allowed(Permission::FsWrite));
        assert!(!p.is_allowed(Permission::Net));
        assert!(p.is_allowed(Permission::Run));
        assert!(p.is_allowed(Permission::Env));
    }

    /// Absent and empty are different answers: only a list that is there
    /// narrows what `net = true` already allows.
    #[test_case("[permissions]\nnet = true\n", None ; "absent_list_stays_unrestricted")]
    #[test_case(
        "[permissions]\nnet = true\nnet_hosts = [\"api.example.com\", \"*.example.com\"]\n",
        Some(vec!["api.example.com".to_owned(), "*.example.com".to_owned()])
        ; "declared_hosts_are_carried"
    )]
    #[test_case("[permissions]\nnet_hosts = []\n", Some(Vec::new()) ; "an_empty_list_reaches_nothing")]
    #[test_case("[permissions]\nnet_hosts = [1]\n", Some(Vec::new()) ; "a_malformed_entry_widens_nothing")]
    fn net_hosts_from_a_manifest(manifest: &str, expected: Option<Vec<String>>) {
        let value: toml::Value = toml::from_str(manifest).unwrap();
        let hosts = PluginPermissions::from_manifest(&value).net_hosts();
        assert_eq!(hosts.as_deref().map(<[String]>::to_vec), expected);
    }

    /// `providers` in the same shape: a list that is not there asks for
    /// nothing, and a malformed entry must not widen what the prompt shows.
    #[test_case("[package]\nname = \"p\"\n", Vec::new() ; "an_absent_list_takes_over_nothing")]
    #[test_case(
        "providers = [\"greenpt\"]\n",
        vec!["greenpt".to_owned()] ;
        "declared_names_are_carried"
    )]
    #[test_case(
        "providers = [\"greenpt\", 7]\n",
        vec!["greenpt".to_owned()] ;
        "a_malformed_entry_widens_nothing"
    )]
    #[test_case("providers = \"greenpt\"\n", Vec::new() ; "not_a_list_takes_over_nothing")]
    fn providers_from_a_manifest(manifest: &str, expected: Vec<String>) {
        let value: toml::Value = toml::from_str(manifest).unwrap();
        assert_eq!(
            Requested::from_manifest(&value).providers(),
            expected.as_slice()
        );
    }

    /// The catalog names a load may take over are the ones the approval
    /// covered, not the ones the manifest merely asks for.
    #[test]
    fn granted_providers_need_both_request_and_approval() {
        const APPROVED: &str = "greenpt";
        const UNAPPROVED: &str = "rival";
        let value: toml::Value =
            toml::from_str(&format!("providers = [{APPROVED:?}, {UNAPPROVED:?}]\n")).unwrap();
        let requested = Requested::from_manifest(&value);

        assert_eq!(
            requested.granted_providers(&[APPROVED.to_owned()]),
            [APPROVED.to_owned()]
        );
        assert_eq!(requested.granted_providers(&[]), Vec::<String>::new());
    }

    #[test]
    fn a_requested_host_list_survives_intersection_with_an_approval() {
        let value: toml::Value =
            toml::from_str("[permissions]\nnet = true\nnet_hosts = [\"api.example.com\"]\n")
                .unwrap();
        let requested = Requested::from_manifest(&value);
        let effective = requested.intersect(&PluginPermissions::trusted());
        assert_eq!(
            effective.net_hosts().as_deref().map(<[String]>::to_vec),
            Some(vec!["api.example.com".to_owned()])
        );
    }

    #[test]
    fn from_manifest_missing_section() {
        let val: toml::Value = toml::from_str("[package]\nname = \"test\"").unwrap();
        let p = PluginPermissions::from_manifest(&val);
        for &perm in Permission::ALL {
            assert!(p.is_allowed(perm), "{perm} should default to allowed");
        }
    }

    #[test]
    fn set_modifies_single_permission() {
        let mut p = PluginPermissions::trusted();
        p.set(Permission::Net, false);
        p.set(Permission::Run, false);
        assert!(p.is_allowed(Permission::FsRead));
        assert!(p.is_allowed(Permission::FsWrite));
        assert!(!p.is_allowed(Permission::Net));
        assert!(!p.is_allowed(Permission::Run));
        assert!(p.is_allowed(Permission::Env));
    }

    #[test]
    fn guard_allowed_calls_inner() {
        let lua = Lua::new();
        let perms = PluginPermissions::trusted();
        let func = perms
            .guard(&[Permission::FsRead], &lua, |_, ()| Ok(42))
            .unwrap();
        let result: i32 = func.call(()).unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn requested_denies_omitted_keys() {
        let val: toml::Value = toml::from_str(
            r#"
            [permissions]
            net = true
            "#,
        )
        .unwrap();
        let req = Requested::from_manifest(&val);
        assert!(req.is_requested(Permission::Net));
        for &perm in Permission::ALL {
            if perm != Permission::Net {
                assert!(!req.is_requested(perm), "{perm} must not be requested");
            }
        }
    }

    #[test]
    fn requested_names_are_the_approval_keys() {
        let value: toml::Value = toml::from_str(
            r#"
            [permissions]
            fs_read = true
            run = true
            "#,
        )
        .unwrap();

        assert_eq!(
            Requested::from_manifest(&value).names(),
            ["fs_read".to_owned(), "run".to_owned()]
        );
    }

    /// The legacy parser stays permissive; only the package parser is strict.
    /// A manifest with no `[permissions]` section proves the two differ.
    #[test]
    fn requested_and_legacy_parsers_disagree_by_design() {
        let val: toml::Value = toml::from_str("[package]\nname = \"p\"").unwrap();
        let legacy = PluginPermissions::from_manifest(&val);
        let requested = Requested::from_manifest(&val);
        for &perm in Permission::ALL {
            assert!(legacy.is_allowed(perm), "legacy stays permissive");
            assert!(!requested.is_requested(perm), "package requests nothing");
        }
    }

    #[test]
    fn intersect_needs_both_request_and_approval() {
        let val: toml::Value = toml::from_str(
            r#"
            [permissions]
            net = true
            run = true
            "#,
        )
        .unwrap();
        let requested = Requested::from_manifest(&val);

        let mut approved = PluginPermissions::denied();
        approved.set(Permission::Net, true);
        approved.set(Permission::FsRead, true);

        let effective = requested.intersect(&approved);
        assert!(
            effective.is_allowed(Permission::Net),
            "requested + approved"
        );
        assert!(!effective.is_allowed(Permission::Run), "not approved");
        assert!(!effective.is_allowed(Permission::FsRead), "not requested");
        assert!(!effective.is_allowed(Permission::Env), "neither");
    }

    #[test]
    fn manual_install_grants_what_it_requests() {
        let val: toml::Value = toml::from_str(
            r#"
            [permissions]
            fs_read = true
            "#,
        )
        .unwrap();
        let granted = Requested::from_manifest(&val).granted();
        assert!(granted.is_allowed(Permission::FsRead));
        assert!(!granted.is_allowed(Permission::Net));
    }

    #[test]
    fn guard_denied_returns_error() {
        let lua = Lua::new();
        let perms = PluginPermissions::denied();
        let func = perms
            .guard(&[Permission::FsRead], &lua, |_, ()| Ok(42))
            .unwrap();
        let err = func.call::<i32>(()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("permission denied"));
        assert!(msg.contains("fs_read"));
    }

    #[test_case("1.2.2", "1.2.3", true; "lower")]
    #[test_case("1.2.3", "1.2.3", true; "equal")]
    #[test_case("1.2.3-alpha.1", "1.2.3-alpha.2", true; "older_prerelease")]
    #[test_case("1.2.3-alpha.2", "1.2.3-alpha.1", false; "newer_prerelease")]
    #[test_case("1.2.4", "1.2.3", false; "higher")]
    fn minimum_version_uses_semver_precedence(required: &str, running: &str, compatible: bool) {
        let required = toml::Value::String(required.to_owned());
        let result = check_minimum_version(PLUGIN, &required, running);
        assert_eq!(result.is_ok(), compatible);
        if !compatible {
            assert!(matches!(result, Err(PluginError::MakiVersionTooOld { .. })));
        }
    }

    #[test]
    fn minimum_version_requires_a_plain_semver_string() {
        let wrong_type = check_minimum_version(PLUGIN, &toml::Value::Integer(1), RUNTIME_VERSION);
        assert!(matches!(
            wrong_type,
            Err(PluginError::InvalidMinimumVersionType { .. })
        ));

        for version in ["not-a-version", "v1.2.3"] {
            let value = toml::Value::String(version.to_owned());
            assert!(matches!(
                check_minimum_version(PLUGIN, &value, RUNTIME_VERSION),
                Err(PluginError::InvalidMinimumVersion { .. })
            ));
        }

        let value = toml::Value::String("1.2.3".to_owned());
        assert!(matches!(
            check_minimum_version(PLUGIN, &value, "invalid"),
            Err(PluginError::InvalidRuntimeVersion { .. })
        ));
    }

    #[test]
    fn manifest_rejects_an_invalid_declared_minimum() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILE),
            format!("{MIN_MAKI_VERSION} = 1\n"),
        )
        .unwrap();
        assert!(matches!(
            check_plugin_compatibility(PLUGIN, Some(dir.path())),
            Err(PluginError::InvalidMinimumVersionType { .. })
        ));

        fs::write(
            dir.path().join(MANIFEST_FILE),
            format!("{MIN_MAKI_VERSION} = \"v1.2.3\"\n"),
        )
        .unwrap();
        assert!(matches!(
            check_plugin_compatibility(PLUGIN, Some(dir.path())),
            Err(PluginError::InvalidMinimumVersion { .. })
        ));
    }

    #[test]
    fn missing_valid_and_malformed_manifests_keep_existing_defaults() {
        assert_denied(&load_plugin_permissions(None));

        let dir = tempfile::tempdir().unwrap();
        assert_denied(&load_plugin_permissions(Some(dir.path())));

        fs::write(dir.path().join(MANIFEST_FILE), "").unwrap();
        let permissions = load_plugin_permissions(Some(dir.path()));
        for &permission in Permission::ALL {
            assert!(permissions.is_allowed(permission));
        }

        fs::write(dir.path().join(MANIFEST_FILE), "not = [valid").unwrap();
        assert_denied(&load_plugin_permissions(Some(dir.path())));
        assert!(
            check_plugin_compatibility(PLUGIN, Some(dir.path())).is_ok(),
            "an unparseable manifest has no floor to enforce"
        );
    }

    #[test]
    fn one_manifest_provides_compatibility_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILE),
            format!("{MIN_MAKI_VERSION} = {RUNTIME_VERSION:?}\n\n[permissions]\nnet = false\n"),
        )
        .unwrap();

        check_plugin_compatibility(PLUGIN, Some(dir.path())).unwrap();
        let permissions = load_plugin_permissions(Some(dir.path()));
        assert!(permissions.is_allowed(Permission::FsRead));
        assert!(!permissions.is_allowed(Permission::Net));
    }
}
