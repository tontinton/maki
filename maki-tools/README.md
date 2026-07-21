# maki-tools

Where each tool runs: parent-side plugin execution vs sandboxed child execution.

## Overview

This crate owns both ends of the sandbox tool story:

- **Parent side** (`bridge.rs`) -- the `code_execution` bridge that runs code inside a `maki-sandbox` child and answers the child's trusted-tool forwards by invoking Lua plugin functions from the host.
- **Child side** (`workload.rs`, `child_lua.rs`) -- the default `maki-sandbox::ChildWorkload`: bash plus Lua-plugin filesystem tools inside the mount namespace, with trusted tools forwarded to the parent over IPC.

Dependency direction stays acyclic:

```
        root (maki binary)
              │
         maki-tools
        ┌────┼─────────┬────────────────┐
        ▼    ▼         ▼                ▼
  maki-sandbox  │   maki-interpreter  maki-lua / maki-agent
   (no deps on  │
   any of these)│
                └── child_lua.rs uses shared helpers from
                    maki-agent (build_tool_input) and maki-lua
                    (json_to_lua, lua_to_json)
```

## Parent side: the code_execution bridge

`run_sandbox_with()` is what the embedding binary installs as its `SandboxRunner`:

```rust
maki_tools::run_sandbox_with(
    &sandbox,
    lua,
    code,
    timeout,
    fns,          // HashMap<String, mlua::Function> -- the host's registered tools
    config_json,  // serialized AgentConfig applied to the child before the run
).await
```

It calls `sandbox.run_code(...)` on a blocking thread, passing a handler that resolves each forwarded trusted-tool name against `fns`, converts the JSON input to Lua (`build_tool_input` + `json_to_lua`), and awaits the plugin function on a coroutine. The result is normalized into an `InterpreterResult { output, stdout }`.

Trusted tool calls arrive on the sandbox IO thread while the run is active; the handler must not block that thread, which is why the run itself happens via `smol::unblock`.

## Child side: the default workload

Call once at process startup, before any sandbox child is forked or re-execed:

```rust
maki_tools::install_child_workload();
```

After `--sandbox-inner` re-exec (same binary), the child builds a `ToolsSession` from this registration. The session assembles one tool map used by both code runs and parent-initiated tool calls:

| Category | Tools | Runs where | Backed by |
|----------|-------|------------|-----------|
| Shell | `bash` | child process | `ChildCtx::exec()` (`fork()+execve()` inside the namespace) |
| Filesystem | `read`, `write`, `edit`, `multiedit`, `glob`, `grep`, `list` | child, inside the mount namespace | `ChildLuaRuntime` Lua plugins |
| Trusted | `webfetch`, `websearch`, `question`, `todo_write`, `task`, `memory`, `skill`, `index` | parent | `ChildCtx::forward_trusted()` over IPC |

Filesystem ops are naturally sandboxed because the plugins run inside the namespace; they only see mounted paths. Trusted tools need host resources (network, UI, agent state) and are forwarded.

### Routing policy (host side)

Which calls reach the child at all is decided in `maki-lua::runtime`, not here. The policy is an allowlist of host tools, `SANDBOX_HOST_TOOLS` (`batch`, `code_execution`, `index`, `memory`, `question`, `skill`, `task`, `todo_write`, `webfetch`, `websearch`) -- everything else routes into the sandbox whenever sandbox mode is on. New tools therefore default to sandboxed execution without touching any list.

A routed tool the child cannot run natively answers with `unknown tool: <name>` (the shared `maki_agent::agent::UNKNOWN_TOOL_PREFIX`). The router treats that as fall-through: it warns and runs the host plugin instead, so tools that are neither local nor trusted-forwardable keep working unchanged.

### Run flow

`ToolsSession::run_code(spec)`:
1. Applies `spec.config` to the runtime's `_config` global.
2. Builds interpreter limits from `timeout_secs` / `max_memory`.
3. Runs the monty interpreter via `runner::run_streaming`, streaming stdout lines to the parent through `ChildCtx::stream_stdout`.
4. Returns a `ChildIoResult`; interpreter errors become the `error` field.

If the Lua runtime fails to initialize in the child, filesystem tools are unavailable but bash still works (warn + degrade, no hard failure).

## ChildLuaRuntime

A minimal `maki.*` API surface so the existing tool plugins load and execute inside the namespace:

- `maki.fs.*` -- filesystem operations (sandboxed by the mount namespace); grep fully implemented
- `maki.uv.*` -- cwd, os_homedir, os_getenv
- `maki.fn.*` -- synchronous process execution (jobstart, jobwait, jobstop)
- `maki.json.*` -- encode/decode (conversion shared with the host runtime: `json_to_lua` / `lua_to_json`)
- `maki.log.*` -- structured logging
- `maki.split` -- string splitting
- `maki.ui.*` -- stubs (no terminal in the sandbox)
- `maki.api.register_tool` / `register_options`
- `maki.treesitter.*` -- stubs
- `maki.async.run` -- runs inline (no async in the child)

Plugins are embedded in the binary at compile time via `include_dir!("$CARGO_MANIFEST_DIR/../plugins")`, so no filesystem mount is needed for them. User plugins load from the sandbox-visible config directory: the child probes `/home/maki/.config/maki` (XDG layout, mounted read-only by the `plugins` profile) and then `/home/maki/.maki/plugins` (legacy layout). The first existing `plugins/` subdir provides the plugin set; otherwise the embedded copies are used. An `init.lua` found in either config dir runs after the embedded plugins load, so custom tools register through the same `maki.api.register_tool` path. `require()` resolves modules from the chosen plugin directory's `lib/` subdirectory or from the embedded sources.

### Tool context (`ctx`)

Lua tool handlers receive `(input, ctx)`, where `ctx` exposes the per-tool state plugins expect:

- `ctx:config(key, default)` -- reads the serialized `AgentConfig` sent from the parent
- `ctx:tool_output_lines()` -- per-tool output line limits
- `ctx:record_read(path)` -- track files read
- `ctx:check_before_edit(path)` -- stale-read check before edits
- `ctx:is_instruction_file(name)` -- instruction-file detection
- `ctx:find_instructions(dir)` -- locate AGENTS.md files (returns `[{path, content}]`)

The `FileReadTracker` starts fresh in the child (safe: `check_before_edit` allows untracked files).

### Result conventions

Handlers return one of:

- `string` -- success output
- `nil, string` -- error message
- table `{ llm_output, is_error? }` -- structured result

`extract_tool_result` normalizes these into `(output, is_error)`. Tool *input* goes through `build_tool_input` (shared with the host-side interpreter) and `json_to_lua` (shared with the host plugin runtime), so argument shapes are identical on both sides of the sandbox boundary. `maki.json.encode` uses the same `lua_to_json` as the host, so tables serialize identically there too.

## Tests

- `src/child_lua.rs` -- unit tests for the child Lua API surface (plugin loading, fs operations, embedded-plugin round-trips)
- `src/workload.rs` -- unit tests for `require_str` argument extraction and plugin-dir discovery
- `tests/run_code.rs` -- end-to-end integration test registering the workload and running code through a real sandbox child (requires Linux namespaces)
