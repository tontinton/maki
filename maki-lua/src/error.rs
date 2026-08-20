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
        "plugins.{plugin} sets options ({keys}), but there is no bundled plugin named \"{plugin}\""
    )]
    UnknownPluginOptions { plugin: String, keys: String },
    #[error("no bundled plugin named \"{plugin}\" (enabled via plugins.{plugin})")]
    UnknownPlugin { plugin: String },
    #[error("plugin host is not running")]
    HostDead,
    #[error("package {name} at {path} has no plugin/*.lua entrypoint")]
    PackageEmpty { name: String, path: PathBuf },
    #[error("package file {path} resolves outside its package directory")]
    PackageEscape { path: PathBuf },
    #[error("{plugin} did not finish loading within {milliseconds}ms")]
    LoadTimeout { plugin: String, milliseconds: u128 },
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
