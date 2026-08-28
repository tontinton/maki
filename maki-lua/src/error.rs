use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("lua error in {plugin}: {source}")]
    Lua {
        plugin: String,
        #[source]
        source: mlua::Error,
    },
    #[error("plugin {plugin} attempted to shadow existing tool '{tool}'")]
    NameConflict { plugin: String, tool: String },
    #[error("io error loading plugin {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "plugin {plugin} has a non-string min_maki_version; use a plain major.minor.patch string"
    )]
    InvalidMinimumVersionType { plugin: String },
    #[error(
        "plugin {plugin} has invalid min_maki_version {version:?}: {source}; use a semantic version such as \"1.2.3\""
    )]
    InvalidMinimumVersion {
        plugin: String,
        version: String,
        #[source]
        source: semver::Error,
    },
    #[error("Maki was built with invalid package version {version:?}: {source}")]
    InvalidRuntimeVersion {
        version: String,
        #[source]
        source: semver::Error,
    },
    #[error("plugin {plugin} requires Maki {required} or newer, but this is Maki {running}")]
    MakiVersionTooOld {
        plugin: String,
        required: semver::Version,
        running: semver::Version,
    },
    #[error("no bundled plugin named \"{plugin}\" (enabled via plugins.{plugin})")]
    UnknownPlugin { plugin: String },
    #[error("plugin host is not running")]
    HostDead,
    #[error("bundled plugin {plugin} has an unusable plugin.toml: {message}")]
    BundledManifest { plugin: String, message: String },
    #[error("package {name} at {path} has no plugin/*.lua entrypoint")]
    PackageEmpty { name: String, path: PathBuf },
    #[error("package file {path} resolves outside its package directory")]
    PackageEscape { path: PathBuf },
    #[error(
        "package \"{name}\" at {path} has the same name as a bundled plugin; \
         rename its directory"
    )]
    PackageNameConflict { name: String, path: PathBuf },
    #[error("package manifest {path} is not valid toml: {message}")]
    PackageManifest { path: PathBuf, message: String },
    #[error("cannot resolve the maki data directory, so no package was looked for: {source}")]
    PackageSiteUnavailable {
        #[source]
        source: io::Error,
    },
    #[error("two packages are both named \"{name}\": {first} and {second}")]
    DuplicatePackage {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },
}

impl PluginError {
    /// Whether the `min_maki_version` floor is what refused the plugin.
    ///
    /// A floor is not a broken plugin, it is one that says this Maki is too
    /// old, so callers that can keep going skip it with a warning instead of
    /// reporting a load failure.
    pub(crate) const fn is_version_floor(&self) -> bool {
        matches!(
            self,
            Self::InvalidMinimumVersionType { .. }
                | Self::InvalidMinimumVersion { .. }
                | Self::InvalidRuntimeVersion { .. }
                | Self::MakiVersionTooOld { .. }
        )
    }
}
