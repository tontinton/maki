use std::collections::HashSet;
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::{CloneFlags, unshare};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, getgid, getuid};
use tracing::{debug, warn};

use crate::apparmor;
use crate::error::SandboxError;
use crate::ipc::{self, SYNC_GO, SYNC_READY};

pub const DEFAULT_ALLOWED_ENV: &[&str] = &["LANG", "TERM", "TMPDIR", "RUST_LOG"];

/// Flags for every tmpfs mounted inside the sandbox. Mirrors what bubblewrap does.
const TMPFS_FLAGS: MsFlags = MsFlags::MS_NOSUID.union(MsFlags::MS_NODEV);

const DEVICE_NAMES: &[&str] = &["null", "zero", "full", "random", "urandom", "console"];

/// Exit codes of the throwaway fork in [`probe`].
const PROBE_OK: i32 = 0;
const PROBE_NO_MOUNTNS: i32 = 1;
const PROBE_NO_USERNS: i32 = 2;

const ENV_DESCRIPTIONS: &[(&str, &str)] = &[
    ("LANG", "locale"),
    ("TERM", "terminal type"),
    ("TMPDIR", "temp directory"),
    ("RUST_LOG", "tracing/log level"),
];

/// A single environment variable that will be available inside the sandbox.
#[derive(Debug, Clone)]
pub struct EnvEntry {
    pub key: String,
    pub value: String,
    pub description: &'static str,
}

#[derive(Debug, Clone)]
pub struct NamespaceConfig {
    pub allowed_env: Vec<String>,
    pub env_vars: Vec<(String, String)>,
    pub workspace_dir: PathBuf,
    pub workspace_name: String,
    pub home_mounts: Vec<(PathBuf, String)>,
    pub readonly_mounts: Vec<(PathBuf, String)>,
    pub path_dirs: Vec<String>,
    /// Extra host directories to bind-mount into the workspace.
    /// Each entry is `(host_path, dir_name_inside_workspace)`.
    pub extra_workspace_dirs: Vec<(PathBuf, String)>,
    /// Host symlinks to recreate inside the sandbox.
    /// Each entry is `(host_symlink_path, sandbox_internal_path)`.
    pub symlinks: Vec<(PathBuf, String)>,
}

fn build_sandbox_path(path_dirs: &[String]) -> String {
    let mut p = String::from("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin");
    if !path_dirs.is_empty() {
        p.push(':');
        p.push_str(&path_dirs.join(":"));
    }
    p
}

fn compute_env_entries(
    allowed_env: &[String],
    env_vars: &[(String, String)],
    path_dirs: &[String],
) -> Vec<EnvEntry> {
    let mut entries: Vec<EnvEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    entries.push(EnvEntry {
        key: "PATH".into(),
        value: build_sandbox_path(path_dirs),
        description: "executable search path",
    });
    entries.push(EnvEntry {
        key: "HOME".into(),
        value: "/home/maki".into(),
        description: "home directory",
    });
    entries.push(EnvEntry {
        key: "USER".into(),
        value: "maki".into(),
        description: "current user",
    });
    seen.extend(["PATH", "HOME", "USER"].map(String::from));

    for &key in DEFAULT_ALLOWED_ENV {
        if seen.contains(key) {
            continue;
        }
        let value = std::env::var(key).unwrap_or_default();
        let description = ENV_DESCRIPTIONS
            .iter()
            .find(|(k, _)| *k == key)
            .map_or("", |(_, d)| *d);
        entries.push(EnvEntry {
            key: key.into(),
            value,
            description,
        });
        seen.insert(key.into());
    }

    for (key, val) in std::env::vars() {
        if key.starts_with("LC_") && !seen.contains(&key) {
            entries.push(EnvEntry {
                description: "locale category",
                key: key.clone(),
                value: val,
            });
            seen.insert(key);
        }
    }

    for key in allowed_env {
        if seen.contains(key.as_str()) {
            continue;
        }
        let value = std::env::var(key).unwrap_or_default();
        entries.push(EnvEntry {
            key: key.clone(),
            value,
            description: "",
        });
        seen.insert(key.clone());
    }

    for (key, val) in env_vars {
        if seen.contains(key.as_str()) {
            if let Some(entry) = entries.iter_mut().find(|e| e.key == *key) {
                entry.value.clone_from(val);
            }
            continue;
        }
        entries.push(EnvEntry {
            key: key.clone(),
            value: val.clone(),
            description: "",
        });
        seen.insert(key.clone());
    }

    entries
}

impl NamespaceConfig {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        allowed_env: Vec<String>,
        env_vars: Vec<(String, String)>,
        workspace_dir: PathBuf,
        workspace_name: String,
        home_mounts: Vec<(PathBuf, String)>,
        readonly_mounts: Vec<(PathBuf, String)>,
        path_dirs: Vec<String>,
        extra_workspace_dirs: Vec<(PathBuf, String)>,
        symlinks: Vec<(PathBuf, String)>,
    ) -> Self {
        Self {
            allowed_env,
            env_vars,
            workspace_dir,
            workspace_name,
            home_mounts,
            readonly_mounts,
            path_dirs,
            extra_workspace_dirs,
            symlinks,
        }
    }

    /// Compute the full list of env vars that will be available in the sandbox,
    /// including default allow-listed vars, LC_* vars from the host, and
    /// user-specified extras.
    #[must_use]
    pub fn effective_env(&self) -> Vec<EnvEntry> {
        compute_env_entries(&self.allowed_env, &self.env_vars, &self.path_dirs)
    }

    /// Replace the child's environment with the filtered allow-listed set.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Env`] if an env key or value contains a null
    /// byte.
    pub fn filter_env(&self) -> Result<(), SandboxError> {
        let mut keep: Vec<(String, String)> = Vec::new();
        for (key, val) in std::env::vars() {
            if self.is_allowed(&key) {
                keep.push((key, val));
            }
        }
        unsafe {
            libc::clearenv();
        }
        let new_path = build_sandbox_path(&self.path_dirs);
        if let Some(p) = keep.iter_mut().find(|(k, _)| k == "PATH") {
            p.1 = new_path;
        } else {
            keep.push(("PATH".to_string(), new_path));
        }

        for (key, val) in &keep {
            let c_key = CString::new(key.as_str())
                .map_err(|_| SandboxError::Env("null byte in env key".into()))?;
            let c_val = CString::new(val.as_str())
                .map_err(|_| SandboxError::Env("null byte in env value".into()))?;
            unsafe {
                libc::setenv(c_key.as_ptr(), c_val.as_ptr(), 1);
            }
        }

        // Override HOME and USER to sandbox-internal values
        unsafe {
            let home = CString::new("/home/maki")
                .map_err(|_| SandboxError::Env("invalid HOME value".into()))?;
            let user =
                CString::new("maki").map_err(|_| SandboxError::Env("invalid USER value".into()))?;
            libc::setenv(c"HOME".as_ptr(), home.as_ptr(), 1);
            libc::setenv(c"USER".as_ptr(), user.as_ptr(), 1);
        }

        debug!(
            home = std::env::var("HOME").unwrap_or_default(),
            user = std::env::var("USER").unwrap_or_default(),
            "sandbox: HOME/USER set"
        );

        debug!(
            path_dirs = self.path_dirs.len(),
            allowed = keep.len(),
            "sandbox: filtered environment variables"
        );
        Ok(())
    }

    fn is_allowed(&self, key: &str) -> bool {
        if DEFAULT_ALLOWED_ENV.contains(&key) {
            return true;
        }
        if key.starts_with("LC_") {
            return true;
        }
        self.allowed_env.iter().any(|e| e == key)
    }

    /// Set up bind mounts and `pivot_root` for filesystem isolation.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Mount`] if any mount or `pivot_root` step
    /// fails.
    pub fn setup_mounts(&self) -> Result<(), SandboxError> {
        setup_mounts_impl(self)
    }

    /// Build a `NamespaceConfig` from agent config fields.
    ///
    /// Shared logic used by both the TUI startup and the UI event loop to
    /// construct sandbox configuration from the agent config. Only the
    /// profiles passed in `enabled_profiles` contribute mounts and PATH
    /// entries; use [`profiles::select_profiles`](crate::profiles::select_profiles)
    /// to resolve configured names.
    pub fn from_agent_config(
        allowed_env: Vec<String>,
        allowed_paths: &[String],
        extra_dirs: &[String],
        enabled_profiles: &[crate::profiles::SandboxProfile],
        workspace_dir: PathBuf,
        workspace_name: String,
    ) -> Self {
        let home = std::env::var("HOME").ok().map(PathBuf::from);
        Self::from_agent_config_with(
            home.as_deref(),
            allowed_env,
            allowed_paths,
            extra_dirs,
            enabled_profiles,
            workspace_dir,
            workspace_name,
        )
    }

    fn from_agent_config_with(
        home: Option<&Path>,
        allowed_env: Vec<String>,
        allowed_paths: &[String],
        extra_dirs: &[String],
        enabled_profiles: &[crate::profiles::SandboxProfile],
        workspace_dir: PathBuf,
        workspace_name: String,
    ) -> Self {
        let mut home_mounts: Vec<(PathBuf, String)> = allowed_paths
            .iter()
            .filter_map(|p| {
                let path = PathBuf::from(p);
                home.and_then(|h| {
                    path.strip_prefix(h).ok().map(|rel| {
                        let name = rel.to_string_lossy().to_string();
                        (path.clone(), name.trim_start_matches('/').to_string())
                    })
                })
            })
            .collect();
        let extra_workspace_dirs: Vec<(PathBuf, String)> = extra_dirs
            .iter()
            .map(|p| {
                let path = PathBuf::from(p);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                (path, name)
            })
            .collect();

        let mut mounted_hosts: HashSet<PathBuf> =
            home_mounts.iter().map(|(p, _)| p.clone()).collect();
        let mut path_dirs: Vec<String> = Vec::new();
        let mut readonly_mounts: Vec<(PathBuf, String)> = Vec::new();
        let mut symlinks: Vec<(PathBuf, String)> = Vec::new();

        if let Some(h) = home {
            let flat = crate::profiles::FlatMounts::from_profiles_under(enabled_profiles, h);
            for (path, name) in flat.home {
                if mounted_hosts.insert(path.clone()) {
                    home_mounts.push((path, name));
                }
            }
            for (path, name) in flat.readonly {
                if mounted_hosts.insert(path.clone()) {
                    readonly_mounts.push((path, name));
                }
            }
            path_dirs.extend(flat.path_dirs);
            symlinks.extend(flat.symlinks);
        }

        Self::new(
            allowed_env,
            vec![],
            workspace_dir,
            workspace_name,
            home_mounts,
            readonly_mounts,
            path_dirs,
            extra_workspace_dirs,
            symlinks,
        )
    }

    /// Drop bind-mount entries whose host source does not exist.
    ///
    /// Opt-in profiles and user config may reference directories that are
    /// absent on this machine; mounting them would fail the whole child
    /// spawn. Missing sources are logged and skipped instead.
    pub fn prune_missing_mounts(&mut self) {
        fn retain_existing(mounts: &mut Vec<(PathBuf, String)>, kind: &str) {
            mounts.retain(|(p, name)| {
                let keep = p.exists();
                if !keep {
                    tracing::warn!(kind, host = %p.display(), target = %name, "sandbox: mount source missing, skipping");
                }
                keep
            });
        }
        retain_existing(&mut self.home_mounts, "home_mount");
        retain_existing(&mut self.readonly_mounts, "readonly_mount");
        retain_existing(&mut self.extra_workspace_dirs, "extra_workspace_dir");
    }

    /// Translate a host path into the equivalent sandbox-side path, based on
    /// the bind-mount layout. Falls back to identity when the path is not
    /// under any mount, so callers pass files that exist outside the sandbox
    /// through unchanged.
    pub fn host_to_sandbox(&self, path: &Path) -> PathBuf {
        let host = path.to_path_buf();
        for (host_mount, sandbox_mount) in self.mount_map() {
            if let Ok(rel) = host.strip_prefix(&host_mount) {
                return sandbox_mount.join(rel);
            }
        }
        host
    }

    /// Translate a sandbox-side path back into the host view. Falls back to
    /// identity for paths outside the mounts.
    pub fn sandbox_to_host(&self, path: &Path) -> PathBuf {
        let sandbox = path.to_path_buf();
        for (sandbox_mount, host_mount) in self.reverse_mount_map() {
            if let Ok(rel) = sandbox.strip_prefix(&sandbox_mount) {
                return host_mount.join(rel);
            }
        }
        sandbox
    }

    fn mount_map(&self) -> Vec<(PathBuf, PathBuf)> {
        let mut pairs = vec![(
            self.workspace_dir.clone(),
            sandbox_workspace_root(&self.workspace_name),
        )];
        for (host, name) in &self.extra_workspace_dirs {
            pairs.push((
                host.clone(),
                PathBuf::from(format!("/home/maki/workspace/{name}")),
            ));
        }
        for (host, name) in self.home_mounts.iter().chain(&self.readonly_mounts) {
            pairs.push((host.clone(), sandbox_home_root(name)));
        }
        sort_longest_first(&mut pairs);
        pairs
    }

    fn reverse_mount_map(&self) -> Vec<(PathBuf, PathBuf)> {
        let mut pairs = vec![(
            sandbox_workspace_root(&self.workspace_name),
            self.workspace_dir.clone(),
        )];
        for (host, name) in &self.extra_workspace_dirs {
            pairs.push((
                PathBuf::from(format!("/home/maki/workspace/{name}")),
                host.clone(),
            ));
        }
        for (host, name) in self.home_mounts.iter().chain(&self.readonly_mounts) {
            pairs.push((sandbox_home_root(name), host.clone()));
        }
        sort_longest_first(&mut pairs);
        pairs
    }
}

fn sandbox_workspace_root(workspace_name: &str) -> PathBuf {
    PathBuf::from(format!("/home/maki/workspace/{workspace_name}"))
}

/// The sandbox side of a home/readonly mount: a trailing slash means the
/// whole `maki` home. Names are relative to `$HOME`, so `name == ""` is the
/// home directory itself.
fn sandbox_home_root(name: &str) -> PathBuf {
    if name.is_empty() {
        PathBuf::from("/home/maki")
    } else {
        PathBuf::from(format!("/home/maki/{name}"))
    }
}

fn sort_longest_first(pairs: &mut [(PathBuf, PathBuf)]) {
    pairs.sort_by_key(|(b, _)| std::cmp::Reverse(b.components().count()));
}

/// Isolate the calling process into a new user namespace, synchronizing with
/// the parent over `sock`.
///
/// # Errors
///
/// Returns a [`SandboxError`] if `unshare(CLONE_NEWUSER)` fails or the sync
/// protocol with the parent breaks.
pub fn isolate_user_ns(sock: &mut UnixStream) -> Result<(), SandboxError> {
    unshare(CloneFlags::CLONE_NEWUSER)
        .map_err(|e| SandboxError::Namespace(format!("unshare(CLONE_NEWUSER): {e}")))?;
    ipc::send_sync(sock, SYNC_READY)?;
    ipc::recv_sync(sock, SYNC_GO)?;
    Ok(())
}

/// Set up a mount namespace so the child gets a private mount tree.
///
/// # Errors
///
/// Returns [`SandboxError::IsolationUnavailable`] when the host refuses the
/// call. There is deliberately no fallback: the bind mounts below are what
/// make the sandbox's paths mean anything, and serving the host filesystem
/// under sandbox paths would quietly break every fs call instead of failing
/// them.
///
/// Call this *after* [`isolate_user_ns`]. Whether the refusal came from
/// AppArmor taking our capabilities away is only observable from inside the
/// new user namespace, where the profile transition has already happened.
pub fn isolate_mount_ns() -> Result<(), SandboxError> {
    unshare(CloneFlags::CLONE_NEWNS).map_err(|e| {
        SandboxError::IsolationUnavailable(format!(
            "{e}\n\n{}",
            diagnose_mount_ns_blocked(apparmor::capabilities_stripped())
        ))
    })?;
    debug!("sandbox: mount namespace isolated");
    Ok(())
}

/// Per-child staging root under /tmp, keyed by the child's pid. A fixed
/// path would let concurrent sandbox children delete each other's
/// mountpoints mid-setup.
fn staging_dir(pid: u32) -> String {
    format!("/tmp/.maki-root-{pid}")
}

/// Host-side staging dir for device nodes, keyed by the child's pid like
/// [`staging_dir`]. Placeholders live here (on the host filesystem), devices
/// are bound over them, and the whole dir is rbound into the sandbox as /dev.
fn device_stage_dir(pid: u32) -> String {
    format!("/tmp/.maki-dev-{pid}")
}

/// Bind host device nodes onto placeholder files in the device stage dir.
/// Per-device failures are warned about and skipped, matching the old
/// bind-into-tmpfs behavior.
fn prepare_device_stage() -> Result<String, SandboxError> {
    let dir = device_stage_dir(std::process::id());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| SandboxError::Mount(format!("create device stage {dir}: {e}")))?;
    for name in DEVICE_NAMES {
        let host_dev = format!("/dev/{name}");
        if !Path::new(&host_dev).exists() {
            continue;
        }
        if let Err(e) = bind_mount_device(Path::new(&host_dev), &format!("{dir}/{name}")) {
            warn!("sandbox: stage /dev/{name} failed ({e}), continuing");
        }
    }
    Ok(dir)
}

/// Remove the staging trees left behind by an exited child. Its mounts died
/// with its mount namespace; only plain directories remain on /tmp.
pub(crate) fn cleanup_staging(pid: Pid) {
    let pid = pid.as_raw().unsigned_abs();
    let _ = std::fs::remove_dir_all(staging_dir(pid));
    let _ = std::fs::remove_dir_all(device_stage_dir(pid));
}

#[allow(clippy::too_many_lines)]
fn setup_mounts_impl(config: &NamespaceConfig) -> Result<(), SandboxError> {
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| SandboxError::Mount(format!("make root private: {e}")))?;
    debug!("sandbox: mount tree made private");

    let staging = staging_dir(std::process::id());
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|e| SandboxError::Mount(format!("create staging {staging}: {e}")))?;

    mount(
        Some("tmpfs"),
        staging.as_str(),
        Some("tmpfs"),
        TMPFS_FLAGS,
        None::<&str>,
    )
    .map_err(|e| SandboxError::Mount(format!("mount tmpfs staging: {e}")))?;

    for dir in &["usr", "etc", "dev", "home/maki", "tmp", "proc"] {
        let path = format!("{staging}/{dir}");
        std::fs::create_dir_all(&path)
            .map_err(|e| SandboxError::Mount(format!("create dir {path}: {e}")))?;
    }
    // Create lib dirs for bind mounts (needed for ELF interpreter resolution)
    for dir in &["lib", "lib64"] {
        let path = format!("{staging}/{dir}");
        std::fs::create_dir_all(&path)
            .map_err(|e| SandboxError::Mount(format!("create dir {path}: {e}")))?;
    }

    // Device nodes are staged on the host filesystem and rbound in as a
    // directory: a device bind-mounted over a file on a tmpfs created inside
    // this user namespace cannot be opened (EACCES), while the same bind on
    // an init-namespace filesystem works.
    let dev_stage = prepare_device_stage()?;
    let dev_path = format!("{staging}/dev");
    mount(
        Some(dev_stage.as_str()),
        dev_path.as_str(),
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| SandboxError::Mount(format!("rbind {dev_stage} -> {dev_path}: {e}")))?;

    // Create /dev symlinks
    let dev_symlinks: &[(&str, &str)] = &[
        ("core", "/proc/kcore"),
        ("fd", "/proc/self/fd"),
        ("ptmx", "pts/ptmx"),
        ("stderr", "/proc/self/fd/2"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
    ];
    for (name, target) in dev_symlinks {
        let link = format!("{dev_path}/{name}");
        if let Err(e) = std::os::unix::fs::symlink(target, &link) {
            warn!("sandbox: symlink /dev/{name} -> {target} failed ({e}), continuing");
        }
    }

    // Mount proc (may fail in user namespace — fall back to bind-mount from host)
    if mount(
        Some("proc"),
        format!("{staging}/proc").as_str(),
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )
    .is_ok()
    {
        debug!("sandbox: proc mounted");
    } else {
        let proc_target = format!("{staging}/proc");
        if mount(
            Some("/proc"),
            proc_target.as_str(),
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .is_ok()
        {
            debug!("sandbox: /proc bind-mounted from host");
        } else {
            let e = std::io::Error::last_os_error();
            warn!("sandbox: /proc mount failed: {e}, continuing without proc");
        }
    }

    let old_root = format!("{staging}/.old_root");
    std::fs::create_dir_all(&old_root)
        .map_err(|e| SandboxError::Mount(format!("create dir {old_root}: {e}")))?;

    bind_mount_ro(Path::new("/usr"), &format!("{staging}/usr"))?;

    // Mount a tmpfs on /etc (empty by default).
    let etc_path = format!("{staging}/etc");
    mount(
        Some("tmpfs"),
        etc_path.as_str(),
        Some("tmpfs"),
        TMPFS_FLAGS,
        None::<&str>,
    )
    .map_err(|e| SandboxError::Mount(format!("mount tmpfs /etc: {e}")))?;
    // /etc/ssl for TLS certificates (directory, not a single file)
    if Path::new("/etc/ssl").is_dir()
        && let Err(e) = bind_mount_ro(Path::new("/etc/ssl"), &format!("{etc_path}/ssl"))
    {
        warn!("sandbox: bind-mount /etc/ssl failed ({e}), continuing");
    }
    // Symlink /bin and /sbin to /usr/bin and /usr/sbin (no ELF interpreter involved)
    symlink_rel("usr/bin", &format!("{staging}/bin"))?;
    symlink_rel("usr/sbin", &format!("{staging}/sbin"))?;

    // The host's /lib and /lib64 are symlinks to usr/lib and usr/lib64, and
    // usr/lib64/ld-linux-x86-64.so.2 is itself a symlink to ../lib/x86_64-linux-gnu/...
    // The kernel's ELF loader can't resolve this chain in a mount namespace.
    // Fix: bind-mount /lib, create /lib64 as tmpfs with the real interpreter.
    // Bind-mount /lib (host /lib is a symlink to usr/lib — resolve the real path)
    let lib_host_path = Path::new("/lib");
    let lib_real_path = match std::fs::read_link(lib_host_path) {
        Ok(target) => {
            let parent = lib_host_path.parent().unwrap_or(Path::new("/"));
            match std::fs::canonicalize(parent.join(&target)) {
                Ok(p) => p,
                Err(_) => lib_host_path.to_path_buf(),
            }
        }
        Err(_) => lib_host_path.to_path_buf(),
    };
    let lib_target = format!("{staging}/lib");
    std::fs::create_dir_all(&lib_target)
        .map_err(|e| SandboxError::Mount(format!("create {lib_target}: {e}")))?;
    bind_mount_ro(&lib_real_path, &lib_target)?;

    // Mount a tmpfs on /lib64 and copy the real interpreter into it, breaking
    // the symlink chain so the kernel's ELF loader can find it directly.
    let lib64_target = format!("{staging}/lib64");
    std::fs::create_dir_all(&lib64_target)
        .map_err(|e| SandboxError::Mount(format!("create {lib64_target}: {e}")))?;
    mount(
        Some("tmpfs"),
        lib64_target.as_str(),
        Some("tmpfs"),
        TMPFS_FLAGS,
        None::<&str>,
    )
    .map_err(|e| SandboxError::Mount(format!("mount tmpfs /lib64: {e}")))?;
    let ld_name = detect_linker_name();
    let ld_src = std::fs::canonicalize(format!("/lib64/{ld_name}"))
        .map_err(|e| SandboxError::Mount(format!("canonicalize {ld_name}: {e}")))?;
    std::fs::copy(&ld_src, format!("{staging}/lib64/{ld_name}"))
        .map_err(|e| SandboxError::Mount(format!("copy {ld_name}: {e}")))?;

    let ws_target = format!("{staging}/home/maki/workspace/{}", config.workspace_name);
    bind_mount_rw(&config.workspace_dir, &ws_target)?;

    for (host_path, name) in &config.extra_workspace_dirs {
        let target = format!("{staging}/home/maki/workspace/{name}");
        bind_mount_rw(host_path, &target)?;
    }

    for (host_path, name) in &config.home_mounts {
        bind_mount_rw(host_path, &format!("{staging}/home/maki/{name}"))?;
    }

    for (host_path, name) in &config.readonly_mounts {
        bind_mount_ro(host_path, &format!("{staging}/home/maki/{name}"))?;
    }

    // Plugins are embedded in the binary — no filesystem mount needed.

    // Recreate host symlinks inside the sandbox.
    for (host_path, sandbox_path) in &config.symlinks {
        match std::fs::read_link(host_path) {
            Ok(target) => {
                let full_target = format!("{staging}{sandbox_path}");
                if let Some(parent) = Path::new(&full_target).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::os::unix::fs::symlink(&target, &full_target) {
                    warn!(
                        host = %host_path.display(),
                        sandbox = sandbox_path,
                        error = %e,
                        "sandbox: create symlink failed"
                    );
                }
            }
            Err(e) => {
                warn!(
                    host = %host_path.display(),
                    error = %e,
                    "sandbox: read symlink target failed"
                );
            }
        }
    }

    mount(
        Some("tmpfs"),
        format!("{staging}/tmp").as_str(),
        Some("tmpfs"),
        TMPFS_FLAGS,
        None::<&str>,
    )
    .map_err(|e| SandboxError::Mount(format!("mount tmpfs /tmp: {e}")))?;

    std::env::set_current_dir(staging.as_str())
        .map_err(|e| SandboxError::Mount(format!("cd to {staging}: {e}")))?;
    pivot_root(staging.as_str(), &old_root)?;

    if let Err(e) = umount2("/.old_root", MntFlags::MNT_DETACH) {
        // The host root stays visible inside the sandbox, so isolation is
        // incomplete; never silent.
        warn!("sandbox: failed to detach /.old_root: {e}");
    }
    let _ = std::fs::remove_dir("/.old_root");

    std::env::set_current_dir(format!("/home/maki/workspace/{}", config.workspace_name))
        .map_err(|e| SandboxError::Mount(format!("cd to workspace: {e}")))?;

    debug!("sandbox: pivot_root into isolated root filesystem");
    Ok(())
}

/// Probe whether user and mount namespaces are available in a throwaway
/// fork, without disturbing the calling process.
///
/// # Errors
///
/// Returns a [`SandboxError`] if the probe socket cannot be created, the
/// fork fails, or the child reports it could not create a namespace.
pub fn probe() -> Result<(), SandboxError> {
    let (mut sync_rx, mut sync_tx) =
        UnixStream::pair().map_err(|e| SandboxError::Ipc(format!("probe socketpair: {e}")))?;

    let pid = unsafe { nix::unistd::fork() }
        .map_err(|e| SandboxError::Fork(format!("probe fork: {e}")))?;
    match pid {
        ForkResult::Child => {
            drop(sync_rx);
            if unshare(CloneFlags::CLONE_NEWUSER).is_err() {
                std::process::exit(PROBE_NO_USERNS);
            }
            sync_tx.write_all(b"\x01").ok();
            let mut buf = [0u8; 1];
            sync_tx.read_exact(&mut buf).ok();
            if unshare(CloneFlags::CLONE_NEWNS).is_ok() {
                std::process::exit(PROBE_OK);
            }
            // Only observable here: the AppArmor profile transition happens as
            // part of the CLONE_NEWUSER above, so the parent is still
            // unconfined and cannot read the child's confinement itself.
            let stripped = u8::from(apparmor::capabilities_stripped());
            sync_tx.write_all(&[stripped]).ok();
            std::process::exit(PROBE_NO_MOUNTNS);
        }
        ForkResult::Parent { child: child_pid } => {
            drop(sync_tx);
            let mut buf = [0u8; 1];
            sync_rx
                .read_exact(&mut buf)
                .map_err(|e| SandboxError::Ipc(format!("probe: read child sync: {e}")))?;
            let uid = getuid();
            let gid = getgid();
            write_uid_map(child_pid, uid.as_raw(), gid.as_raw())?;
            sync_rx
                .write_all(b"\x01")
                .map_err(|e| SandboxError::Ipc(format!("probe: write child sync: {e}")))?;
            let mut caps_stripped = [0u8; 1];
            sync_rx.read_exact(&mut caps_stripped).ok();
            let exit_code = match waitpid(child_pid, None) {
                Ok(WaitStatus::Exited(_, code)) => code,
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    return Err(SandboxError::Ipc(format!(
                        "probe child killed by signal {sig}"
                    )));
                }
                Ok(_) => {
                    return Err(SandboxError::Ipc(
                        "probe child exited with unexpected status".into(),
                    ));
                }
                Err(e) => {
                    return Err(SandboxError::Ipc(format!("probe waitpid: {e}")));
                }
            };
            match exit_code {
                PROBE_OK => Ok(()),
                PROBE_NO_MOUNTNS => Err(SandboxError::Namespace(diagnose_mount_ns_blocked(
                    caps_stripped[0] != 0,
                ))),
                PROBE_NO_USERNS => Err(SandboxError::Namespace(
                    "user namespace (CLONE_NEWUSER) is not available on this system.\n\
                     Without user namespaces the sandbox cannot isolate processes.\n\n\
                     To use --sandbox, try:\n\
                     - Run maki as root: sudo maki --sandbox\n\
                     - Enable user namespaces: echo 1 > /proc/sys/kernel/unprivileged_userns_clone"
                        .into(),
                )),
                sig if sig & 0x7f != 0 => Err(SandboxError::Ipc(format!(
                    "probe child killed by signal {}",
                    sig & 0x7f
                ))),
                _ => Err(SandboxError::Ipc(format!(
                    "probe child exited with unexpected code {exit_code}"
                ))),
            }
        }
    }
}

/// Host-side facts that explain a refused `unshare(CLONE_NEWNS)`.
///
/// Every field is a recorded answer, never a live probe: [`diagnose`] reads
/// this struct and nothing else, so a test can drive any combination of them
/// without owning the host.
#[derive(Debug, Default, PartialEq, Eq)]
struct Host {
    /// The sandbox landed in AppArmor's `unprivileged_userns` profile, which
    /// allows the user namespace but denies every capability. Observed inside
    /// the new user namespace, since that is where the transition happens.
    apparmor_stripped_caps: bool,
    /// The host arms the AppArmor userns restriction at all.
    apparmor_restrict: bool,
    /// The active kernel lockdown mode, if the host reports one.
    lockdown: Option<String>,
    /// The container runtime hosting us, if any.
    container: Option<Container>,
}

impl Host {
    /// Collect the host's own answers. `caps_stripped` is the only input that
    /// cannot come from this process: the AppArmor profile transition happened
    /// in the child, inside the user namespace.
    fn read(caps_stripped: bool) -> Self {
        Self {
            apparmor_stripped_caps: caps_stripped,
            apparmor_restrict: apparmor::restrict_userns_enabled(),
            lockdown: read_lockdown(),
            container: read_container(),
        }
    }
}

/// The container runtime the host's own hints name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Podman,
    Docker,
    Lxc,
    Other,
}

impl Container {
    #[must_use]
    const fn label(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Docker => "docker",
            Self::Lxc => "lxc",
            Self::Other => "an OCI",
        }
    }

    /// Map a runtime name, as the `container` environment variable, systemd's
    /// `container` file, or a cgroup path reports it.
    fn from_name(name: &str) -> Option<Self> {
        match name.trim() {
            "podman" => Some(Self::Podman),
            "docker" | "containerd" | "kubernetes" => Some(Self::Docker),
            "lxc" | "lxc-libvirt" | "systemd-nspawn" => Some(Self::Lxc),
            "oci" | "rkt" => Some(Self::Other),
            _ => None,
        }
    }

    /// Map the fragments a runtime leaves in `/proc/1/cgroup`. Podman writes
    /// `libpod-<id>`, docker and containerd write their own name, and
    /// Kubernetes writes `kubepods`.
    fn from_cgroup(cgroup: &str) -> Option<Self> {
        if cgroup.contains("libpod") || cgroup.contains("podman") {
            Some(Self::Podman)
        } else if cgroup.contains("docker")
            || cgroup.contains("containerd")
            || cgroup.contains("kubepods")
        {
            Some(Self::Docker)
        } else if cgroup.contains("lxc") {
            Some(Self::Lxc)
        } else {
            None
        }
    }
}

/// Podman's in-container marker. Empty, so presence is the whole signal.
const PODMAN_MARKER: &str = "/run/.containerenv";
/// Docker's in-container marker.
const DOCKER_MARKER: &str = "/.dockerenv";
/// What systemd-nspawn and `systemd-detect-virt -c` write: the runtime name.
const SYSTEMD_CONTAINER: &str = "/run/systemd/container";
/// The OCI-standard environment variable naming the runtime.
const CONTAINER_ENV: &str = "container";
const CGROUP_PATH: &str = "/proc/1/cgroup";
const LOCKDOWN_PATH: &str = "/sys/kernel/security/lockdown";

/// Resolve the container runtime from the host's own hints, most specific
/// first: a runtime marker file beats the name a runtime exported, which beats
/// guessing from a cgroup path.
///
/// Pure so tests do not have to run inside a container. `read` answers for the
/// filesystem and returns `None` for anything absent, and `container_env`
/// carries the `container` variable.
fn detect_container(
    read: &dyn Fn(&str) -> Option<String>,
    container_env: Option<&str>,
) -> Option<Container> {
    if read(PODMAN_MARKER).is_some() {
        return Some(Container::Podman);
    }
    if read(DOCKER_MARKER).is_some() {
        return Some(Container::Docker);
    }
    if let Some(runtime) = read(SYSTEMD_CONTAINER)
        && let Some(container) = Container::from_name(&runtime)
    {
        return Some(container);
    }
    if let Some(runtime) = container_env
        && let Some(container) = Container::from_name(runtime)
    {
        return Some(container);
    }
    read(CGROUP_PATH).and_then(|cgroup| Container::from_cgroup(&cgroup))
}

/// The lockdown mode the host reports between brackets, e.g. `integrity`.
/// The file lists every supported mode with the active one bracketed:
/// `none [integrity] confidentiality`.
fn parse_lockdown(content: &str) -> Option<String> {
    content
        .split_whitespace()
        .find(|word| word.starts_with('[') && word.ends_with(']'))
        .map(|word| word[1..word.len() - 1].to_owned())
}

/// Lockdown modes that block the namespace syscalls the sandbox depends on.
const LOCKDOWN_BLOCKING_MODES: &[&str] = &["integrity", "confidentiality"];

fn lockdown_blocks_userns(mode: Option<&str>) -> bool {
    mode.is_some_and(|mode| LOCKDOWN_BLOCKING_MODES.contains(&mode))
}

fn read_container() -> Option<Container> {
    detect_container(
        &|path| std::fs::read_to_string(path).ok(),
        std::env::var(CONTAINER_ENV).ok().as_deref(),
    )
}

fn read_lockdown() -> Option<String> {
    std::fs::read_to_string(LOCKDOWN_PATH)
        .ok()
        .as_deref()
        .and_then(parse_lockdown)
}

fn diagnose_mount_ns_blocked(caps_stripped: bool) -> String {
    diagnose(&Host::read(caps_stripped))
}

fn diagnose(host: &Host) -> String {
    let mut msg = String::from(
        "Mount namespaces (CLONE_NEWNS) are blocked on this system.\n\
         The sandbox builds its filesystem inside a user namespace it creates itself,\n\
         so unshare(CLONE_NEWNS) needs CAP_SYS_ADMIN there. That call just failed.\n\n\
         Detected:\n",
    );

    let mut any_clue = false;

    if host.apparmor_stripped_caps {
        msg.push_str(
            "  - AppArmor confined the sandbox to 'unprivileged_userns', which grants\n\
             \x20   the user namespace but denies every capability inside it\n",
        );
        any_clue = true;
    }

    if host.apparmor_restrict {
        msg.push_str("  - kernel.apparmor_restrict_unprivileged_userns=1\n");
        any_clue = true;
    }

    if lockdown_blocks_userns(host.lockdown.as_deref()) {
        msg.push_str("  - kernel lockdown=");
        msg.push_str(host.lockdown.as_deref().unwrap_or_default());
        msg.push_str(" (usually enabled by Secure Boot)\n");
        any_clue = true;
    }

    if let Some(container) = host.container {
        msg.push_str("  - running inside a ");
        msg.push_str(container.label());
        msg.push_str(" container\n");
        any_clue = true;
    }

    if !any_clue {
        msg.push_str("  - (unrecognized kernel restriction)\n");
    }

    msg.push_str("\nTo use --sandbox, try one of:\n");

    if host.apparmor_stripped_caps || host.apparmor_restrict {
        msg.push_str(&apparmor::install_instructions(
            &apparmor::sandbox_binaries()
                .iter()
                .map(PathBuf::as_path)
                .collect::<Vec<_>>(),
        ));
    }

    if host.apparmor_restrict && !host.apparmor_stripped_caps {
        // No `unprivileged_userns` profile is loaded, so AppArmor denies the
        // namespace outright and no profile of ours will attach until one is.
        msg.push_str("  - Load AppArmor's bundled profile for unprivileged user namespaces:\n");
        msg.push_str("      sudo apt install apparmor\n");
    }

    if host.apparmor_restrict {
        msg.push_str(
            "  - Last resort, weakening every program on the host rather than just maki:\n",
        );
        msg.push_str("      sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0\n");
        msg.push_str("    Or make permanent: add 'kernel.apparmor_restrict_unprivileged_userns=0' to /etc/sysctl.conf\n");
    }

    if lockdown_blocks_userns(host.lockdown.as_deref()) {
        msg.push_str("  - Disable Secure Boot in your BIOS/UEFI settings, then reboot\n");
        msg.push_str("  - Or add 'lockdown=none' to the kernel cmdline:\n");
        msg.push_str("      Edit /etc/default/grub: GRUB_CMDLINE_LINUX=\"$GRUB_CMDLINE_LINUX lockdown=none\"\n");
        msg.push_str("      Then: sudo update-grub && reboot\n");
    }

    if let Some(container) = host.container {
        msg.push_str(&format!(
            "  - Run the container with --privileged ({} run --privileged)\n",
            container.label()
        ));
        msg.push_str("  - Run maki on the host system directly\n");
    }

    msg.push_str("  - Run maki as root: sudo maki --sandbox\n");
    msg
}

// ── Helpers ──

fn pivot_root(new_root: &str, put_old: &str) -> Result<(), SandboxError> {
    let new =
        CString::new(new_root).map_err(|_| SandboxError::Mount("pivot_root new_root".into()))?;
    let old =
        CString::new(put_old).map_err(|_| SandboxError::Mount("pivot_root put_old".into()))?;
    let ret = unsafe { libc::syscall(libc::SYS_pivot_root, new.as_ptr(), old.as_ptr()) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        return Err(SandboxError::Mount(format!("pivot_root: {err}")));
    }
    Ok(())
}

fn symlink_rel(target: &str, link: &str) -> Result<(), SandboxError> {
    std::os::unix::fs::symlink(target, link)
        .map_err(|e| SandboxError::Mount(format!("symlink {link} -> {target}: {e}")))
}

fn detect_linker_name() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "ld-linux-x86-64.so.2"
    } else if cfg!(target_arch = "aarch64") {
        "ld-linux-aarch64.so.1"
    } else if cfg!(target_arch = "riscv64") {
        "ld-linux-riscv64-lp64d.so.1"
    } else {
        "ld-linux-x86-64.so.2"
    }
}

fn bind_mount(source: &Path, target: &str, readonly: bool) -> Result<(), SandboxError> {
    // If the source doesn't exist, create its parent directory so the mount
    // can proceed (e.g. ~/.local/maki when ~/.local doesn't exist yet).
    if !source.exists()
        && let Some(parent) = source.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            SandboxError::Mount(format!("create parent of {}: {e}", source.display()))
        })?;
    }
    std::fs::create_dir_all(target)
        .map_err(|e| SandboxError::Mount(format!("create mount target {target}: {e}")))?;
    mount(
        Some(source),
        target,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| SandboxError::Mount(format!("bind {} -> {}: {e}", source.display(), target)))?;
    if readonly {
        mount(
            None::<&str>,
            target,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_RDONLY | MsFlags::MS_REMOUNT | MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(|e| {
            SandboxError::Mount(format!(
                "remount ro {} -> {}: {e}",
                source.display(),
                target
            ))
        })?;
    }
    Ok(())
}

fn bind_mount_rw(source: &Path, target: &str) -> Result<(), SandboxError> {
    bind_mount(source, target, false)
}

fn bind_mount_ro(source: &Path, target: &str) -> Result<(), SandboxError> {
    bind_mount(source, target, true)
}

/// Bind-mount a device file from host into the sandbox.
/// Unlike `bind_mount`, creates a regular file (not a directory) at the target
/// so that `mount` can overlay a character/block device over it.
fn bind_mount_device(source: &Path, target: &str) -> Result<(), SandboxError> {
    std::fs::File::create(target)
        .map_err(|e| SandboxError::Mount(format!("create {target}: {e}")))?;
    mount(
        Some(source),
        target,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| SandboxError::Mount(format!("bind {} -> {}: {e}", source.display(), target)))?;
    Ok(())
}

/// Write the uid/gid mapping for a user-namespace child so its root maps to
/// the given host uid/gid.
///
/// # Errors
///
/// Returns [`SandboxError::Namespace`] if writing `setgroups`, `uid_map`, or
/// `gid_map` fails.
pub fn write_uid_map(child_pid: Pid, uid: u32, gid: u32) -> Result<(), SandboxError> {
    let uid_map_path = format!("/proc/{child_pid}/uid_map");
    let gid_map_path = format!("/proc/{child_pid}/gid_map");
    let setgroups_path = format!("/proc/{child_pid}/setgroups");
    std::fs::write(&setgroups_path, b"deny\n")
        .map_err(|e| SandboxError::Namespace(format!("write setgroups: {e}")))?;
    std::fs::write(&uid_map_path, format!("0 {uid} 1\n"))
        .map_err(|e| SandboxError::Namespace(format!("write uid_map: {e}")))?;
    std::fs::write(&gid_map_path, format!("0 {gid} 1\n"))
        .map_err(|e| SandboxError::Namespace(format!("write gid_map: {e}")))?;
    debug!("sandbox: wrote uid/gid map for child pid {child_pid}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use test_case::test_case;

    fn host(stripped: bool, lockdown: Option<&str>, container: Option<Container>) -> Host {
        Host {
            apparmor_stripped_caps: stripped,
            apparmor_restrict: stripped,
            lockdown: lockdown.map(str::to_owned),
            container,
        }
    }

    #[test_case(true, Some("integrity"), None; "apparmor")]
    #[test_case(true, Some("none"), None; "apparmor_with_harmless_lockdown")]
    #[test_case(false, None, Some(Container::Podman); "podman_container")]
    #[test_case(false, Some("confidentiality"), None; "lockdown")]
    #[test_case(false, None, None; "unknown")]
    fn diagnosis_names_every_detected_restriction(
        stripped: bool,
        lockdown: Option<&str>,
        container: Option<Container>,
    ) {
        let msg = diagnose(&host(stripped, lockdown, container));
        assert!(
            msg.contains("Mount namespaces (CLONE_NEWNS) are blocked"),
            "the failing call must be named: {msg}"
        );
        if stripped {
            assert!(
                msg.contains("unprivileged_userns"),
                "the profile that stripped our capabilities is the cause: {msg}"
            );
            assert!(
                msg.contains("Install maki's AppArmor profile"),
                "the profile is the fix, matching how bwrap works: {msg}"
            );
        }
        if lockdown_blocks_userns(lockdown) {
            assert!(
                msg.contains("lockdown="),
                "lockdown must be reported: {msg}"
            );
        }
        if let Some(container) = container {
            assert!(
                msg.contains(&format!("running inside a {} container", container.label())),
                "{msg}"
            );
        }
        if !stripped && lockdown.is_none() && container.is_none() {
            assert!(msg.contains("unrecognized kernel restriction"), "{msg}");
        }
    }

    #[test]
    fn apparmor_restriction_prefers_the_profile_over_the_sysctl() {
        let msg = diagnose(&host(true, None, None));
        let profile = msg.find("Install maki's AppArmor profile").unwrap();
        let sysctl = msg
            .find("sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0")
            .unwrap();
        assert!(
            profile < sysctl,
            "weakening the host must be the last resort, not the first suggestion: {msg}"
        );
        assert!(msg.contains("stays at 1"), "{msg}");
    }

    #[test]
    fn restriction_without_the_userns_profile_also_names_the_missing_one() {
        let mut host = host(false, None, None);
        host.apparmor_restrict = true;
        let msg = diagnose(&host);
        // No transition means AppArmor's bundled unprivileged_userns profile is
        // not loaded, so it denies the namespace outright. Our profile cannot
        // attach until the system one is there.
        assert!(
            msg.contains("sudo apt install apparmor"),
            "the missing system profile must be named: {msg}"
        );
        assert!(
            msg.contains("Install maki's AppArmor profile"),
            "capabilities stay denied either way, so our profile is still needed: {msg}"
        );
    }

    #[test]
    fn a_podman_container_gets_its_own_advice() {
        let msg = diagnose(&host(false, None, Some(Container::Podman)));
        assert!(
            msg.contains("running inside a podman container"),
            "the runtime must be named, not just 'a container': {msg}"
        );
        assert!(msg.contains("podman run --privileged"), "{msg}");
    }

    #[test_case(Some("integrity"), true; "integrity")]
    #[test_case(Some("confidentiality"), true; "confidentiality")]
    #[test_case(Some("none"), false; "none")]
    #[test_case(None, false; "absent")]
    fn only_lockdown_modes_that_block_namespaces_are_reported(mode: Option<&str>, blocks: bool) {
        assert_eq!(lockdown_blocks_userns(mode), blocks);
    }

    #[test]
    fn lockdown_mode_is_the_bracketed_one() {
        assert_eq!(
            parse_lockdown("none [integrity] confidentiality").as_deref(),
            Some("integrity")
        );
        assert_eq!(
            parse_lockdown("none integrity confidentiality").as_deref(),
            None,
            "nothing bracketed means no mode is active"
        );
    }

    /// [`detect_container`] against a stand-in filesystem holding only
    /// `files`, so no test has to run inside a container.
    fn container_from(files: &[(&str, &str)], container_env: Option<&str>) -> Option<Container> {
        let read = |path: &str| -> Option<String> {
            files
                .iter()
                .find(|(name, _)| *name == path)
                .map(|(_, body)| (*body).to_owned())
        };
        detect_container(&read, container_env)
    }

    #[test]
    fn detects_podman_from_its_marker_file() {
        assert_eq!(
            container_from(&[(PODMAN_MARKER, "")], None),
            Some(Container::Podman)
        );
    }

    #[test]
    fn detects_podman_from_a_rootless_cgroup() {
        let cgroup = "0::/user.slice/user-1000.slice/user@1000.service/user.slice/libpod-8f1.scope";
        assert_eq!(
            container_from(&[(CGROUP_PATH, cgroup)], None),
            Some(Container::Podman)
        );
    }

    #[test]
    fn detects_docker_from_its_marker_file() {
        assert_eq!(
            container_from(&[(DOCKER_MARKER, "")], None),
            Some(Container::Docker)
        );
    }

    #[test]
    fn detects_the_runtime_systemd_records() {
        assert_eq!(
            container_from(&[(SYSTEMD_CONTAINER, "podman\n")], None),
            Some(Container::Podman)
        );
    }

    #[test]
    fn detects_the_runtime_exported_as_an_environment_variable() {
        assert_eq!(container_from(&[], Some("docker")), Some(Container::Docker));
    }

    #[test]
    fn a_marker_beats_any_other_hint() {
        assert_eq!(
            container_from(
                &[(PODMAN_MARKER, ""), (CGROUP_PATH, "0::/init.scope")],
                None
            ),
            Some(Container::Podman)
        );
    }

    #[test]
    fn reports_no_container_on_a_bare_host() {
        assert_eq!(
            container_from(&[(CGROUP_PATH, "0::/init.scope")], None),
            None
        );
        assert_eq!(container_from(&[], None), None);
    }

    fn config_with_env(allowed: Vec<&str>) -> NamespaceConfig {
        NamespaceConfig::new(
            allowed.into_iter().map(String::from).collect(),
            vec![],
            PathBuf::from("/workspace"),
            "test".into(),
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        )
    }

    #[test]
    fn is_allowed_default_env() {
        let cfg = config_with_env(vec![]);
        assert!(cfg.is_allowed("LANG"));
        assert!(cfg.is_allowed("TERM"));
        assert!(cfg.is_allowed("TMPDIR"));
        assert!(cfg.is_allowed("RUST_LOG"));
    }

    #[test]
    fn is_allowed_lc_prefix() {
        let cfg = config_with_env(vec![]);
        assert!(cfg.is_allowed("LC_ALL"));
        assert!(cfg.is_allowed("LC_MESSAGES"));
    }

    #[test]
    fn is_allowed_custom() {
        let cfg = config_with_env(vec!["MY_VAR", "DEBUG"]);
        assert!(cfg.is_allowed("MY_VAR"));
        assert!(cfg.is_allowed("DEBUG"));
        assert!(!cfg.is_allowed("RANDOM_UNSET"));
    }

    #[test]
    fn is_allowed_not_in_default_list() {
        let cfg = config_with_env(vec![]);
        assert!(!cfg.is_allowed("HOME"));
        assert!(!cfg.is_allowed("PATH"));
        assert!(!cfg.is_allowed("SOME_RANDOM_VAR"));
    }

    #[test]
    fn detect_linker_name_returns_known_arch() {
        let name = detect_linker_name();
        assert!(!name.is_empty());
        assert!(name.starts_with("ld-linux"));
    }

    #[test]
    fn effective_env_contains_hardcoded_vars() {
        let cfg = config_with_env(vec![]);
        let env = cfg.effective_env();
        let keys: Vec<&str> = env.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"PATH"));
        assert!(keys.contains(&"HOME"));
        assert!(keys.contains(&"USER"));
    }

    #[test]
    fn effective_env_home_is_sandbox_path() {
        let cfg = config_with_env(vec![]);
        let env = cfg.effective_env();
        let home = env.iter().find(|e| e.key == "HOME").unwrap();
        assert_eq!(home.value, "/home/maki");
    }

    #[test]
    fn effective_env_includes_default_allow_list() {
        let cfg = config_with_env(vec![]);
        let env = cfg.effective_env();
        let keys: Vec<&str> = env.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"LANG"));
        assert!(keys.contains(&"TERM"));
        assert!(keys.contains(&"TMPDIR"));
        assert!(keys.contains(&"RUST_LOG"));
    }

    #[test]
    fn effective_env_includes_custom_allowed() {
        let cfg = config_with_env(vec!["MY_VAR"]);
        let env = cfg.effective_env();
        let keys: Vec<&str> = env.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"MY_VAR"));
    }

    #[test]
    fn effective_env_custom_env_vars_override() {
        let mut cfg = config_with_env(vec![]);
        cfg.env_vars = vec![("MY_VAR".into(), "hello".into())];
        let env = cfg.effective_env();
        let my_var = env.iter().find(|e| e.key == "MY_VAR").unwrap();
        assert_eq!(my_var.value, "hello");
    }

    #[test]
    fn effective_env_custom_env_vars_override_existing() {
        let mut cfg = config_with_env(vec!["LANG"]);
        cfg.env_vars = vec![("LANG".into(), "en_US.UTF-8".into())];
        let env = cfg.effective_env();
        let lang = env.iter().find(|e| e.key == "LANG").unwrap();
        assert_eq!(lang.value, "en_US.UTF-8");
    }

    #[test]
    fn effective_env_path_includes_path_dirs() {
        let mut cfg = config_with_env(vec![]);
        cfg.path_dirs = vec!["/home/maki/.cargo/bin".into()];
        let env = cfg.effective_env();
        let path = env.iter().find(|e| e.key == "PATH").unwrap();
        assert!(path.value.contains("/home/maki/.cargo/bin"));
        assert!(path.value.contains("/usr/bin"));
    }

    #[test]
    fn build_sandbox_path_empty_dirs() {
        assert_eq!(
            build_sandbox_path(&[]),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin"
        );
    }

    #[test]
    fn build_sandbox_path_with_dirs() {
        let dirs = vec!["/home/maki/.cargo/bin".into(), "/opt/bin".into()];
        let result = build_sandbox_path(&dirs);
        assert!(
            result.starts_with("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:"),
            "profile dirs must come after the defaults, got {result}"
        );
        assert!(result.contains("/home/maki/.cargo/bin"));
        assert!(result.contains("/opt/bin"));
    }

    #[test]
    fn effective_env_no_duplicate_keys() {
        let mut cfg = config_with_env(vec!["LANG", "MY_VAR"]);
        cfg.env_vars = vec![
            ("MY_VAR".into(), "new".into()),
            ("EXTRA".into(), "val".into()),
        ];
        let env = cfg.effective_env();
        let mut keys: Vec<&str> = env.iter().map(|e| e.key.as_str()).collect();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "env entries must have unique keys");
    }

    #[test]
    fn effective_env_ordering() {
        let cfg = config_with_env(vec![]);
        let env = cfg.effective_env();
        let keys: Vec<&str> = env.iter().map(|e| e.key.as_str()).collect();
        let path_idx = keys.iter().position(|&k| k == "PATH").unwrap();
        let home_idx = keys.iter().position(|&k| k == "HOME").unwrap();
        let user_idx = keys.iter().position(|&k| k == "USER").unwrap();
        assert!(path_idx < home_idx, "PATH must come before HOME");
        assert!(home_idx < user_idx, "HOME must come before USER");
        // Hardcoded vars before defaults
        let lang_idx = keys.iter().position(|&k| k == "LANG").unwrap();
        assert!(user_idx < lang_idx, "USER must come before LANG");
    }

    #[test]
    fn effective_env_new_var_via_env_vars() {
        let cfg = NamespaceConfig::new(
            vec![],
            vec![("MY_CUSTOM_VAR".into(), "custom_value".into())],
            PathBuf::from("/workspace"),
            "test".into(),
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        let env = cfg.effective_env();
        let entry = env
            .iter()
            .find(|e| e.key == "MY_CUSTOM_VAR")
            .expect("MY_CUSTOM_VAR should be present");
        assert_eq!(entry.value, "custom_value");
    }

    #[test]
    fn effective_env_duplicate_env_vars_last_wins() {
        let cfg = NamespaceConfig::new(
            vec![],
            vec![
                ("DUP".into(), "first".into()),
                ("DUP".into(), "second".into()),
            ],
            PathBuf::from("/workspace"),
            "test".into(),
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        let env = cfg.effective_env();
        let entries: Vec<_> = env.iter().filter(|e| e.key == "DUP").collect();
        assert_eq!(
            entries.len(),
            1,
            "duplicate env_vars keys should produce one entry"
        );
        assert_eq!(entries[0].value, "second", "last value should win");
    }

    #[test]
    fn compute_env_entries_standalone() {
        let env = compute_env_entries(&[], &[], &[]);
        let keys: Vec<&str> = env.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"PATH"));
        assert!(keys.contains(&"HOME"));
        assert!(keys.contains(&"USER"));
    }

    #[test]
    fn from_agent_config_basic() {
        let config = NamespaceConfig::from_agent_config_with(
            None,
            vec!["MY_VAR".into()],
            &["/home/user/.local/maki".into()],
            &["/host/extras".into()],
            &[],
            PathBuf::from("/workspace"),
            "myproject".into(),
        );
        assert_eq!(config.workspace_dir, PathBuf::from("/workspace"));
        assert_eq!(config.workspace_name, "myproject");
        assert!(config.allowed_env.contains(&"MY_VAR".to_string()));
        assert_eq!(config.extra_workspace_dirs.len(), 1);
        assert_eq!(
            config.extra_workspace_dirs[0],
            (PathBuf::from("/host/extras"), "extras".into())
        );
    }

    #[test]
    fn from_agent_config_empty() {
        let config = NamespaceConfig::from_agent_config_with(
            None,
            vec![],
            &[],
            &[],
            &[],
            PathBuf::from("/ws"),
            "ws".into(),
        );
        assert!(config.home_mounts.is_empty());
        assert!(config.readonly_mounts.is_empty());
        assert!(config.path_dirs.is_empty());
        assert!(config.extra_workspace_dirs.is_empty());
        assert!(config.allowed_env.is_empty());
    }

    #[test]
    fn from_agent_config_dedupes_allowed_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cargo/bin")).unwrap();
        let allowed = tmp.path().join(".cargo");
        let config = NamespaceConfig::from_agent_config_with(
            Some(tmp.path()),
            vec![],
            &[allowed.to_string_lossy().into_owned()],
            &[],
            &[],
            PathBuf::from("/ws"),
            "ws".into(),
        );
        let cargo_mounts = config
            .home_mounts
            .iter()
            .filter(|(p, _)| p == &allowed)
            .count();
        assert_eq!(cargo_mounts, 1, "allowed path must not be mounted twice");
    }

    #[test]
    fn from_agent_config_applies_only_enabled_profiles() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cargo/bin")).unwrap();
        std::fs::create_dir(tmp.path().join(".rustup")).unwrap();
        std::fs::create_dir(tmp.path().join("go")).unwrap();
        let rust_only: Vec<_> = crate::profiles::select_profiles(&["rust".into()]);
        let config = NamespaceConfig::from_agent_config_with(
            Some(tmp.path()),
            vec![],
            &[],
            &[],
            &rust_only,
            PathBuf::from("/ws"),
            "ws".into(),
        );
        assert_eq!(
            config.path_dirs,
            vec!["/home/maki/.cargo/bin".to_string()],
            "enabled rust profile must extend the sandbox PATH"
        );
        assert!(
            config
                .home_mounts
                .contains(&(tmp.path().join(".cargo"), ".cargo".into()))
        );
        assert!(
            !config
                .home_mounts
                .contains(&(tmp.path().join("go"), "go".into())),
            "disabled go profile must not contribute mounts even though ~/go exists"
        );
    }

    #[test]
    fn from_agent_config_without_profiles_mounts_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cargo/bin")).unwrap();
        let config = NamespaceConfig::from_agent_config_with(
            Some(tmp.path()),
            vec![],
            &[],
            &[],
            &[],
            PathBuf::from("/ws"),
            "ws".into(),
        );
        assert!(config.home_mounts.is_empty());
        assert!(config.readonly_mounts.is_empty());
        assert!(config.path_dirs.is_empty());
    }

    #[test]
    fn prune_missing_mounts_drops_absent_sources() {
        let tmp = tempfile::TempDir::new().unwrap();
        let present = tmp.path().join("present");
        std::fs::create_dir(&present).unwrap();
        let mut config = NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from("/ws"),
            "ws".into(),
            vec![
                (present.clone(), "present".into()),
                (tmp.path().join("absent"), "absent".into()),
            ],
            vec![(tmp.path().join("gone_ro"), "gone_ro".into())],
            vec![],
            vec![(tmp.path().join("nope"), "nope".into())],
            vec![],
        );
        config.prune_missing_mounts();
        assert_eq!(config.home_mounts, vec![(present, "present".into())]);
        assert!(config.readonly_mounts.is_empty());
        assert!(config.extra_workspace_dirs.is_empty());
    }

    #[test]
    fn host_to_sandbox_maps_workspace() {
        let config = NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from("/ws"),
            "proj".into(),
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(
            config.host_to_sandbox(&PathBuf::from("/ws/src/main.rs")),
            PathBuf::from("/home/maki/workspace/proj/src/main.rs")
        );
        assert_eq!(
            config.host_to_sandbox(&PathBuf::from("/ws")),
            PathBuf::from("/home/maki/workspace/proj")
        );
    }

    #[test]
    fn host_to_sandbox_maps_extra_workspace_dirs() {
        let config = NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from("/ws"),
            "proj".into(),
            vec![],
            vec![],
            vec![],
            vec![(PathBuf::from("/host/extras"), "extras".into())],
            vec![],
        );
        assert_eq!(
            config.host_to_sandbox(&PathBuf::from("/host/extras/lib.rs")),
            PathBuf::from("/home/maki/workspace/extras/lib.rs")
        );
    }

    #[test]
    fn host_to_sandbox_maps_home_and_readonly() {
        let config = NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from("/ws"),
            "proj".into(),
            vec![(PathBuf::from("/home/user/.cargo"), ".cargo".into())],
            vec![(PathBuf::from("/home/user/.rustup"), ".rustup".into())],
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(
            config.host_to_sandbox(&PathBuf::from("/home/user/.cargo/bin/cargo")),
            PathBuf::from("/home/maki/.cargo/bin/cargo")
        );
        assert_eq!(
            config.host_to_sandbox(&PathBuf::from("/home/user/.rustup/toolchains")),
            PathBuf::from("/home/maki/.rustup/toolchains")
        );
    }

    #[test]
    fn host_to_sandbox_unmounted_paths_pass_through() {
        let config = NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from("/ws"),
            "proj".into(),
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(
            config.host_to_sandbox(&PathBuf::from("/etc/hosts")),
            PathBuf::from("/etc/hosts")
        );
    }

    #[test]
    fn sandbox_to_host_roundtrips_workspace() {
        let config = NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from("/ws"),
            "proj".into(),
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(
            config.sandbox_to_host(&PathBuf::from("/home/maki/workspace/proj/src/main.rs")),
            PathBuf::from("/ws/src/main.rs")
        );
    }

    #[test]
    fn sandbox_to_host_roundtrips_home() {
        let config = NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from("/ws"),
            "proj".into(),
            vec![(PathBuf::from("/home/user/.cargo"), ".cargo".into())],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(
            config.sandbox_to_host(&PathBuf::from("/home/maki/.cargo/bin/cargo")),
            PathBuf::from("/home/user/.cargo/bin/cargo")
        );
    }

    #[test]
    fn sandbox_to_host_unmounted_paths_pass_through() {
        let config = NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from("/ws"),
            "proj".into(),
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(
            config.sandbox_to_host(&PathBuf::from("/usr/bin/sh")),
            PathBuf::from("/usr/bin/sh")
        );
    }

    #[test]
    fn sandbox_to_host_unrelated_sandbox_workspace_does_not_match() {
        let config = NamespaceConfig::new(
            vec![],
            vec![],
            PathBuf::from("/ws"),
            "proj".into(),
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(
            config.sandbox_to_host(&PathBuf::from("/home/maki/workspace/other/file")),
            PathBuf::from("/home/maki/workspace/other/file")
        );
    }
}
