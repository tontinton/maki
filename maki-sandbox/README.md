# maki-sandbox

Linux namespace-based sandbox providing process isolation, a minimal isolated filesystem, and an IPC transport. The child runs a persistent IO loop that serves filesystem operations and shell commands; maki's Lua plugins stay on the host and only their fs/exec operations cross the boundary.

## Overview

`maki-sandbox` owns the isolation layer: user and mount namespaces, a minimal read-only root filesystem built from the host's `/usr`, `/lib`, and `/dev`, a writable workspace bind mount, environment filtering, and a Unix-socket IPC protocol between parent and child.

It deliberately does **not** own an execution engine. Every filesystem operation the child serves (read, write, metadata, dir, mkdir, rm, mv, glob, grep) is pure Rust in `child.rs`. The search operations come from `maki_fs::search`, the same ripgrep/`ignore`-powered walks `HostFs` uses, so results stay byte-identical between the two sides. The crate stays free of mlua, interpreter, and agent dependencies.

The host side exposes the child through [`Sandbox`](src/sandbox.rs) and [`SandboxFs`](src/fs_backend.rs), the latter implementing `maki_fs::FsBackend`. `maki`'s `src/cmd/tui.rs` installs a `SandboxFs` as the routed backend, so tools in a routed call execute against the sandbox's filesystem while the Lua plugin code itself runs in the host runtime.

## Process model

```
 maki (parent)                     maki --sandbox-inner (child, post-exec)
 ┌─────────────────────┐           ┌──────────────────────────────────┐
 │                     │           │                                  │
 │  Sandbox struct     │  socket   │  InnerChild                      │
 │  ├── ls/pwd/cd/exec │◄─────────►│  ├── serve ParentMsg loop        │
 │  ├── fs(op)         │           │  ├── answer Ls/Pwd/Cd/Exec/Fs    │
 │  ├── reinit()       │           │  └── answer Fs ops (incl. search) │
 │  └── wait()         │           │                                  │
 │                     │           │                                  │
 └─────────────────────┘           └──────────────────────────────────┘
          │
          │ fork()
          ▼
 maki (outer child, short-lived)
 ┌─────────────────────────────┐
 │  SandboxChild               │
 │  ├── filter_env()           │
 │  ├── unshare(CLONE_NEWUSER) │
 │  ├── unshare(CLONE_NEWNS)   │
 │  ├── setup_mounts()         │
 │  ├── pivot_root()           │
 │  └── exec(/proc/self/exe    │
 │       --sandbox-inner)      │
 └─────────────────────────────┘
```

Three processes are involved:

1. **Parent** (`Sandbox`) -- the main maki process. Sends requests over the socket; an IO thread (`sandbox-parent-io`) routes replies back to the pending waiter for each call id.
2. **Outer child** (`SandboxChild`) -- a short-lived process forked by `spawn_child`. Sets up namespaces, builds the mount tree, does `pivot_root`, then execs `/proc/self/exe --sandbox-inner` so the inner instance starts with a clean process state inside the isolated filesystem.
3. **Inner child** (`InnerChild`) -- the post-exec process that runs inside the isolated root. It serves a persistent IO loop: `Ls`, `Pwd`, `Cd`, and `Exec` answered inline, and every `Fs` op from `child.rs`.

## Filesystem ops

The child's filesystem operations are Rust (no Lua, no python) in `child.rs` and served through the `Fs` request:

- read (`with_byte_limit`), read bytes, write, metadata (null on any error), dir (`path, max_depth`), make dir, remove (recursive or forced), and move.

Search operations (multi-pattern glob with gitignore/mtime, grep with a single pattern, include glob, context lines, per-line byte cap) are `maki_fs::search::glob_walk` and `maki_fs::search::grep_search` -- the shared implementations behind `FsBackend::glob`/`FsBackend::grep`, so the sandboxed and host backends return identical results.

Paths in the child are sandbox-side absolute paths; `SandboxFs` translates between host and sandbox views (`NamespaceConfig::host_to_sandbox` / `sandbox_to_host`).

## IPC protocol

Communication uses a Unix socket pair with length-prefixed JSON messages (4-byte big-endian length header + payload). Max message size is 16 MB.

### Startup sequence

```
Parent                          Child
  │                               │
  │──── Handshake {name,ver} ────►│
  │◄─── Handshake {name,ver} ─────│
  │                               │
  │         (child unshares user ns)
  │                               │
  │◄────── sync "ready" ─────────│
  │     (parent writes uid_map)   │
  │─────── sync "go" ────────────►│
  │      (child unshares mount ns, sets up mounts, pivot_root)
  │                               │
  │◄── Setup { error: none } ─────│
  │   (parent spawns the IO thread and returns the Sandbox)
```

The `Setup` reply is what makes `Sandbox::new` honest: the parent blocks on it,
so a host that refuses `unshare(CLONE_NEWNS)` surfaces as a constructor error
rather than a `Sandbox` whose paths silently do not resolve. A failed setup
replies with the child's own `SandboxError` so the variant survives the socket.

### Message types

Every request carries a `call_id`; the child echoes it in the matching response so results route by id.

**Parent -> Child** (`ParentMsg`):
- `Ls { path }`, `Pwd`, `Cd { path }` -- filesystem queries answered by the child IO loop
- `Exec { command }` -- run a shell command inside the isolated filesystem (raw `fork()`+`exec()` via `posix_spawnp` fails inside user+mount namespaces)
- `Fs { op }` -- a filesystem operation (`read`, `write`, `metadata`, ...)
- `Exit` -- shut down the child

**Child -> Parent** (`ChildMsg`):
- `Setup { error }` -- sent once, after the namespaces and mounts are in place. `error: none` means the child is ready; otherwise it carries the setup error and the parent abandons the spawn.
- `LsResult { entries }`, `PwdResult { path }`, `CdResult`, `ExecResult { output, exit_code }`, `FsResult { result }` -- replies matched by call id

After the `Setup` reply the child has nothing left to announce: a later failure
just closes the socket, and the parent's read error fails every pending waiter.

## Filesystem layout

Inside the mount namespace, the child sees:

```
/                   tmpfs (staging root)
├── usr/            bind-mounted from host (read-only)
├── bin -> usr/bin  symlink
├── sbin -> usr/sbin symlink
├── lib/            bind-mounted from host (read-only, resolves ELF loader)
├── lib64/          tmpfs with real ld-linux copied in (breaks symlink chain)
├── etc/            tmpfs (empty, except /etc/ssl bind-mounted from host
│                   and host symlinks recreated, e.g. localtime, alternatives/cc)
├── dev/            rbind of a host-staged device dir (/tmp/.maki-dev-{pid}):
│                   devices are bound onto placeholder files there first,
│                   because a device bound over a file on a tmpfs created
│                   inside the user namespace cannot be opened (EACCES)
├── proc/           procfs (or bind-mounted from host if procfs mount fails)
├── tmp/            tmpfs (scratch space)
└── home/maki/
    └── workspace/
        └── {name}/ bind-mounted from host (read-write, the working directory)
```

Host directories from profiles and `sandbox_allowed_paths` are bind-mounted under `/home/maki/`.

## Namespace isolation

- **User namespace** (`CLONE_NEWUSER`): maps the current uid/gid to root inside the child. Required for all other namespace operations.
- **Mount namespace** (`CLONE_NEWNS`): gives the child its own mount tree. Required. `Sandbox::new` waits for the child to finish this setup and returns `SandboxError::IsolationUnavailable` if the host refuses, so callers never get a sandbox whose paths do not resolve. There is no unisolated mode.

## Environment

The child's environment is wiped (`clearenv`) and rebuilt from scratch. Only these variables pass through:

- `PATH` -- rebuilt as `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin` plus profile PATH entries
- `HOME` -- always `/home/maki`
- `USER` -- always `maki`
- Default allowed: `LANG`, `TERM`, `TMPDIR`, `RUST_LOG`
- Any `LC_*` variables from the host
- User-specified `sandbox_allowed_env` entries

## Public API

```rust
let sandbox = Sandbox::new(config)?;                // fork + namespace setup (Arc)
let out = sandbox.exec("ls -la", timeout, env)?;    // (output, exit_code)
let entries = sandbox.ls("/home/maki")?;            // directory listing
let pwd = sandbox.pwd()?;                           // child cwd
sandbox.cd("/tmp")?;
let reply = sandbox.fs(FsOp::Read { path, byte_limit })?;
sandbox.reinit(new_config)?;                        // tear down + respawn
sandbox.exit()?;                                    // send exit signal, then wait on drop
```

`SandboxFs::new(Arc<Sandbox>)` wraps the handle as a `maki_fs::FsBackend` for the maki-lua routing layer. All IPC is serialized through internal mutexes. `reinit` tears down the old child (sends `Exit`, waits) before spawning a new one.

## Profiles

Profiles are named collections of host directories that can be toggled per project. Only enabled profiles contribute mounts and PATH entries; they are enabled in the Sandbox dialog or via `agent.sandbox_profiles = ["rust", "go"]` in `.maki/config.toml`, and that choice persists. Mount sources missing on the host are skipped with a warning instead of failing the spawn.

Built-in profiles:

| Name      | Mounts                                         |
|-----------|-------------------------------------------------|
| `rust`    | `~/.cargo` (rw), `~/.cargo/bin` (PATH), `~/.rustup` (ro) |
| `c/c++`   | `/etc/alternatives/cc` and `/etc/alternatives/c++` (symlink) -- cargo needs a runnable `cc`, and the Debian/Ubuntu alternatives chain dangles inside the sandbox |
| `java`    | `~/.m2` (rw), `~/.gradle` (rw)                |
| `node`    | `~/.npm` (rw), `~/.yarn` (rw), `~/.npm/bin` (PATH) |
| `go`      | `~/go` (rw), `~/go/bin` (PATH)                |
| `plugins` | `~/.config/maki` (ro), `~/.maki/plugins` (ro) -- custom Maki plugins inside the sandbox |

Mount usages are rw, read-only, PATH entry, or recreated symlink. Symlinks are
resolved on the host (`read_link`) and recreated at the same path in the
sandbox, so they only work when their target is visible there too.

Use `profiles::select_profiles()` to resolve configured names against the built-ins, and `profiles::build_namespace_config()` to convert enabled profiles into a `NamespaceConfig`.

## Binaries

- `sandbox-shell` -- interactive CLI for testing the sandbox. Supports `--profile`, `--exec-only`, and `--list-profiles` flags.
- `sandbox-diag` -- diagnostic tool that probes namespace support, filesystem layout, and exec behavior to diagnose why sandbox commands may fail.

## Tests

- `src/child.rs` -- unit tests for `list_dir_entries`
- `src/ipc.rs` -- roundtrip tests for all IPC message types
- `src/namespace.rs` -- tests for env computation, path building, linker detection, profile application to `NamespaceConfig`, and pruning of missing mount sources
- `src/profiles.rs` -- tests for path resolution, profile-to-config conversion
- `tests/browse.rs` -- file browser integration test
- `tests/exec.rs` -- shell execution integration test
- `tests/search.rs` -- glob and grep through a real namespace-backed sandbox
- `tests/common/mod.rs` -- shared `sandbox_for` helper

The integration tests need user namespace support: `CLONE_NEWUSER` must be
unprivileged (`/proc/sys/kernel/unprivileged_userns_clone=1` on most distros),
`CLONE_NEWNS` must be allowed, and where AppArmor restricts unprivileged user
namespaces the `maki-sandbox` profile must be loaded (see `src/apparmor.rs`).
Where the host cannot isolate, `sandbox_for` prints the
reason and returns `None` so the test passes without asserting. Any other
construction failure is a defect and panics.

## AppArmor

When `kernel.apparmor_restrict_unprivileged_userns=1` is armed, that sysctl is
not what blocks the sandbox. What blocks it is that AppArmor moves an unconfined
process into the `unprivileged_userns` profile the moment it calls
`unshare(CLONE_NEWUSER)`, and that profile denies every capability. The sandbox
needs `CAP_SYS_ADMIN` inside the user namespace it just created, so
`unshare(CLONE_NEWNS)` returns `EPERM`.

`bwrap` works with the sysctl left at `1` because AppArmor ships
`/etc/apparmor.d/bwrap-userns-restrict`, a profile granting `userns`,
`capability` and `mount` for `/usr/bin/bwrap`. `src/apparmor.rs` renders the
equivalent profile for maki's binaries and prints the two commands that install
it. Loading policy needs `CAP_MAC_ADMIN`, so it stays a one-time root step --
the same one the `apparmor` package performs for `bwrap`. Turning the sysctl off
also works, but it weakens every program on the host, not just maki.