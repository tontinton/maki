# maki-sandbox

Linux namespace-based sandbox providing process isolation and an IPC transport for running untrusted workloads.

## Overview

`maki-sandbox` owns everything below the workload: user and mount namespaces, a minimal read-only root filesystem built from the host's `/usr`, `/lib`, and `/dev`, a writable workspace bind mount, environment filtering, and a Unix-socket IPC protocol between parent and child.

It deliberately does **not** own an execution engine. What runs inside the child is injected at startup through the [`ChildWorkload`](src/workload.rs) seam (see [Workload injection](#workload-injection)). The default workload -- bash plus Lua-plugin filesystem tools plus trusted-tool forwarding -- lives in the separate `maki-tools` crate, keeping this crate free of mlua, interpreter, and agent dependencies.

## Process model

```
 maki (parent)                     maki --sandbox-inner (child, post-exec)
 ┌─────────────────────┐           ┌──────────────────────────────────────┐
 │                     │           │                                      │
 │  Sandbox struct     │  socket   │  InnerChild                         │
 │  ├── ls/pwd/cd/exec │◄─────────►│  ├── build ChildSession (registry)  │
 │  ├── run_code()     │           │  ├── serve Run on worker thread     │
 │  ├── call_tool()    │           │  ├── answer ToolCall locally        │
 │  └── wait()         │           │  └── stream Stdout/Done             │
 │                     │           │                                      │
 └─────────────────────┘           └──────────────────────────────────────┘
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

1. **Parent** (`Sandbox`) -- the main maki process. Sends `Run`, queries, and tool calls over the socket; an IO thread (`parent_io_thread`) routes replies and dispatches trusted tool calls forwarded by the child to the handler installed by the active `run_code` call.

2. **Outer child** (`SandboxChild`) -- a short-lived process forked by `spawn_child`. Sets up namespaces, builds the mount tree, does `pivot_root`, then execs `/proc/self/exe --sandbox-inner` so the inner instance starts with a clean process state inside the isolated filesystem. If exec fails, it falls back to running the inner loop in-place.

3. **Inner child** (`InnerChild`) -- the post-exec process that runs inside the isolated root. It builds its `ChildSession` from the workload registry (survives the re-exec because the registry is a process-global in the same binary) and serves it on a worker thread. The IO thread answers `Ls`, `Pwd`, `Cd`, and `Exec` directly, without involving the session.

## Workload injection

The child's execution engine is supplied by the embedding binary via a process-global registry:

```rust
// once at startup, before any child is forked or re-execed
maki_sandbox::register_child_workload(Arc::new(MyWorkload));
```

```rust
pub trait ChildWorkload: Send + Sync {
    /// Called once per child process, before any IPC traffic is served.
    fn init(&self, ctx: ChildCtx) -> Result<Box<dyn ChildSession>, String>;
}

pub trait ChildSession: Send {
    fn run_code(&mut self, spec: RunSpec) -> ChildIoResult;
    fn handle_tool_call(&mut self, name: &str, args: Vec<Value>, kwargs: Vec<(String, Value)>)
        -> Result<String, String>;
}
```

- `RunSpec` carries `call_id`, `code`, `timeout_secs`, `max_memory`, and the serialized config JSON.
- `ChildCtx` is the session's handle back to the outside world: `stream_stdout()` streams run output, `forward_trusted()` runs a tool in the parent over IPC (request/reply with a timeout backstop), and `exec()` runs a shell command inside the isolated filesystem.
- If no workload is registered (the `sandbox-diag` and `sandbox-shell` binaries don't register one), a fallback `NoWorkloadSession` is used: runs fail lazily with "no child workload registered", while browse queries (`ls`/`pwd`/`cd`/`exec`) still work because they never reach the session.

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
  │         (child continues)     │
```

### Message types

Every request carries a `call_id`; the child echoes it in the matching response so results route by id.

**Parent -> Child** (`ParentMsg`):
- `Run { call_id, code, timeout_secs, max_memory, config }` -- execute code in the child's workload
- `ToolCall { call_id, name, args, kwargs }` -- parent-initiated tool call for local execution
- `ToolResult { call_id, result }` -- reply to a forwarded trusted tool call
- `Cancel` -- cancel pending forwarded calls
- `Exit` -- shut down the child
- `Ls { path }`, `Pwd`, `Cd { path }`, `Exec { command }` -- filesystem queries answered by the IO thread

**Child -> Parent** (`ChildMsg`):
- `Stdout { call_id, text }` -- streaming stdout chunk
- `Done { call_id, output, stdout, error }` -- final result of a `Run`
- `ToolCall { call_id, name, args, kwargs }` -- trusted tool forwarded for parent execution
- `ToolResult { call_id, result }` -- result of a parent-initiated tool call
- `LsResult { entries }`, `PwdResult { path }`, `CdResult`, `ExecResult { output, is_error }`

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
- **Mount namespace** (`CLONE_NEWNS`): gives the child its own mount tree. If unavailable (e.g. AppArmor restrictions), the child falls back to running without filesystem isolation.

## Environment

The child's environment is wiped (`clearenv`) and rebuilt from scratch. Only these variables pass through:

- `PATH` -- rebuilt as `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin` plus profile PATH entries
- `HOME` -- always `/home/maki`
- `USER` -- always `maki`
- Default allowed: `LANG`, `TERM`, `TMPDIR`, `RUST_LOG`
- Any `LC_*` variables from the host
- User-specified `sandbox_allowed_env` entries

## Public API

The main entry point is `Sandbox`, an `Arc`-shared handle:

```rust
let sandbox = Sandbox::new(config)?;              // fork + namespace setup
let out = sandbox.run_code(                       // run code in the child's workload
    code,
    timeout_secs,
    max_memory,
    config_json,
    |name, args, kwargs| { /* answer forwarded trusted tools */ },
)?;
let result = sandbox.call_tool("bash", args, kwargs)?; // parent-initiated tool call
let pwd = sandbox.pwd()?;                          // query child state
let entries = sandbox.ls("/home/maki")?;           // list directory
sandbox.cd("/tmp")?;                               // change child cwd
sandbox.reinit(new_config)?;                       // tear down + respawn
sandbox.exit()?;                                   // send exit signal
// child is waited on when Sandbox is dropped
```

Trusted tools forwarded by the child are answered by the `handler` closure of the active `run_code` call; calls arriving outside a run fail with "no sandbox run is active". All IPC is serialized through internal mutexes. `reinit` tears down the old child (sends `Exit`, waits) before spawning a new one.

## Profiles

Profiles are named collections of host directories that can be toggled per project. Only enabled profiles contribute mounts and PATH entries; they are enabled in the Sandbox dialog or via `agent.sandbox_profiles = ["rust", "go"]` in `.maki/config.toml`, and that choice persists. Mount sources missing on the host are skipped with a warning instead of failing the spawn.

Built-in profiles:

| Name      | Mounts                                         |
|-----------|-------------------------------------------------|
| `rust`    | `~/.cargo` (rw), `~/.cargo/bin` (PATH), `~/.rustup` (ro), `/etc/alternatives/cc` (symlink -- cargo needs a runnable `cc`, and the Debian/Ubuntu alternatives chain dangles inside the sandbox) |
| `java`    | `~/.m2` (rw), `~/.gradle` (rw)                |
| `node`    | `~/.npm` (rw), `~/.yarn` (rw), `~/.npm/bin` (PATH) |
| `go`      | `~/go` (rw), `~/go/bin` (PATH)                |
| `plugins` | `~/.config/maki` (ro), `~/.maki/plugins` (ro) -- custom Maki plugins inside the sandbox |

Mount usages are rw, read-only, PATH entry, or recreated symlink. Symlinks are
resolved on the host (`read_link`) and recreated at the same path in the
sandbox, so they only work when their target is visible there too.

Use `profiles::select_profiles()` to resolve configured names against the built-ins, and `profiles::build_namespace_config()` to convert enabled profiles into a `NamespaceConfig`.

## Binaries

- `sandbox-shell` -- interactive CLI for testing the sandbox. Supports `--profile`, `--exec-only`, and `--list-profiles` flags. Runs without a registered workload (browse queries only).
- `sandbox-diag` -- diagnostic tool that probes namespace support, filesystem layout, and exec behavior to diagnose why sandbox commands may fail.

## Tests

- `src/child.rs` -- unit tests for `list_dir_entries`
- `src/ipc.rs` -- roundtrip tests for all IPC message types
- `src/namespace.rs` -- tests for env computation, path building, linker detection, profile application to `NamespaceConfig`, and pruning of missing mount sources
- `src/profiles.rs` -- tests for path resolution, profile-to-config conversion
- `tests/browse.rs` -- file browser integration test
- `tests/exec.rs` -- shell execution integration test

Integration tests that exercise a full code run live in `maki-tools` (`tests/run_code.rs`), since they need a registered workload.
