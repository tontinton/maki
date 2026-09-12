+++
title = "Permissions"
weight = 6
[extra]
group = "Reference"
+++

# Permissions

Maki uses a permission system to decide what each tool is allowed to do and when to ask you first.

Whether a project's `.maki` configuration loads at all is a separate question,
answered once per folder. See [folder trust](/docs/folder-trust/).

## Rule Layers

Rules come from four layers, combined for resolution:

1. **Session rules**, set during the current session (in-memory only)
2. **Config rules**, loaded from TOML permission files
3. **Builtin rules**, the hardcoded defaults
4. **Plugin rules**, declared by plugins via [`maki.api.register_permission_rule`](/docs/lua-api/#maki-api-register_permission_rule)

Any matching deny blocks the tool. No exceptions, so a config deny always beats a plugin allow.

A [`tool.<name>.input` hook](/docs/hooks/) runs before any of this. Rules are
resolved against the call as the hook left it, so what the prompt shows you is
what runs.

## Check Flow

For every tool call, each scope resolves like this:

```
tool call
    │
deny rule matches?  ── yes ──►  blocked. no exceptions
    │ no
allow rule matches? ── yes ──►  runs
    │ no
YOLO active?        ── yes ──►  runs
    │ no
plan file write?    ── yes ──►  runs
    │ no
    ▼
default: prompt / allow / deny
```

Deny rules are checked across all layers before anything else, so a deny cannot be bypassed by YOLO or the plan-file auto-allow. In plan mode, writes to any path other than the plan file are rejected before this flow; this applies to the file-write tools only. All other tools, including MCP tools, follow the check flow below as usual. `default` resolves per-tool first, then global; the built-in default is `"prompt"`.

## Builtin Defaults

File-write tools are pre-allowed inside the project working directory (cwd at session start, canonicalized). Paths outside that tree still need a prompt or an explicit allow rule:

| Tool | Scope | Notes |
|------|-------|-------|
| `write` | `<cwd>/**` | Outside cwd requires permission |
| `edit` | `<cwd>/**` | Outside cwd requires permission |
| `multiedit` | `<cwd>/**` | Outside cwd requires permission |
| `edit_lines` | `<cwd>/**` | Outside cwd requires permission |
| `insert_lines` | `<cwd>/**` | Same, when the opt-in tool is enabled |
| `task` | `*` | Subagent spawning always allowed |

The memory plugin uses a plugin rule to pre-allow the file-write tools inside its notes directory (under maki's state dir), so the agent can edit memory notes directly without a prompt.

These tools have no builtin allow rule, so they prompt (or follow your `default`) every time unless you add rules:

- `bash` - Shell commands (scopes come from tree-sitter parsing)
- `websearch` - Web search queries
- `webfetch` - URL fetching

Tools that never declare permission scopes (for example `read`, `glob`, `grep`, `index`, `memory`, `skill`, `todo_write`) **skip** the permission manager entirely. They always run. If you need to block one of them, turn the plugin off in `init.lua` (`plugins.read = { enabled = false }`) rather than using `permissions.toml`.

Container tools like `batch` and `code_execution` prompt for each inner tool individually.

## TOML Configuration

There are two permission files:

- **Global**: `~/.config/maki/permissions.toml`
- **Project**: `.maki/permissions.toml` in the active Git checkout, or in the
  working directory outside Git (takes precedence over global)

The project file's `deny` scopes always apply. The rest of it waits on
[folder trust](/docs/folder-trust/).

```toml
default = "deny"

[bash]
allow = [
    "cargo *",
    "git *",
]
deny = [
    "rm -rf *",
    "sudo *",
]

[read]
default = "allow"

[mcp.deepwiki]
allow = ["search", "fetch"]

[mcp.github]
deny = ["admin_delete"]
```

Each tool gets its own section with `allow` and `deny` arrays. Values are glob-like scope patterns.

> **Note:** In MCP server sections (`[mcp.*]`), the boolean forms `allow = true` and `deny = true` are deprecated and ignored. Use `default = "allow"` or `default = "deny"` instead. For native tool sections (e.g. `[bash]`), `allow = true` still works.

### The `default` key

Controls what happens when no allow or deny rule matches. Can be `"prompt"` (built-in default), `"deny"`, or `"allow"`. Set it globally or per-tool:

```toml
default = "deny"

[bash]
default = "prompt"
allow = ["cargo *"]
```

Here everything is denied by default, except `bash` which still prompts, and `cargo *` commands which are allowed.

Project files **cannot** set `default = "allow"` (top-level, per-tool, or MCP).
That value is ignored so a repository cannot grant itself full access. Project
**allow lists** work once the folder is [trusted](/docs/folder-trust/). Put
`default = "allow"` only in the global file.

## Scope Patterns

| Pattern | Matches |
|---------|--------|
| `*` or `**` | Any value (full wildcard) |
| `prefix*` | Values starting with prefix |
| `cmd *` | Bare `cmd` or `cmd` plus args (`pwd *` matches `pwd` and `pwd -L`, not `pwdx`) |
| `dir/**` | `dir` itself or anything under it (path-aware on Windows and Unix) |
| `exact` | Exact match only |

## MCP Tool Permissions

MCP tools use natural TOML nesting. Server names are table keys under `[mcp]`, tool names are array values:

```toml
# Global permissions.toml (default = "allow" is ignored in project files)
[mcp.deepwiki]
allow = ["search", "fetch"]

[mcp.github]
deny = ["admin_delete"]

[mcp.lean-lsp]
default = "allow"               # allow all tools on this server (global only)
```

Tool names must match `^[a-zA-Z0-9_-]{1,64}$` (no dots, max 64 chars). Server names cannot contain dots.

## Permission Prompts

When a gated tool needs permission, Maki asks you.

| Key | Action |
|-----|--------|
| `y` | Allow once (immediate) |
| `s` | Allow for this session (confirm with `Enter` or `y`; any other key cancels) |
| `a` | Always allow for this project (confirm; saved to `.maki/permissions.toml`) |
| `A` | Always allow globally (confirm; saved to `~/.config/maki/permissions.toml`) |
| `n` | Open deny guidance editor (type optional guidance, then `Enter` to deny once; `Esc` cancels) |
| `d` | Deny always for this project (confirm; saved to `.maki/permissions.toml`) |
| `D` | Deny always globally (confirm) |

Session and always-allow / always-deny choices need a second key (`Enter` or `y`) so a fat-finger does not rewrite your rules. Deny-once with `n` lets you type a short reason the agent will see.

The keys are the same in a folder you have not
[trusted](/docs/folder-trust/), where `a` and `d` last for the session instead
of reaching `.maki/permissions.toml`.

ACP clients offer the four options the protocol defines. "Allow always" lasts
for the session, and "Reject always" is a project answer that follows folder
trust like the TUI, reading "Reject for this session" in an untrusted folder.

### Scope Generalization

When you pick "always allow" (or always deny for MCP), the saved scope is generalized so it stays useful beyond that one call:

- **bash**: `cargo test --all` becomes `cargo *`
- **write / edit / multiedit / edit_lines / insert_lines**: `/path/to/file.rs` becomes `/path/to/**`
- **MCP tools**: always `*` (per-tool, so allowing `deepwiki.search` will not cover `deepwiki.fetch`)
- **webfetch / websearch** (and anything else gated): the exact URL or query string is stored as-is

For MCP tools, both allow and deny decisions generalize to `*` (the entire tool). MCP inputs are opaque JSON with no meaningful scope pattern. Denying a single MCP invocation denies that tool until you revoke the rule.

## YOLO Mode

To skip prompts on gated tools, toggle YOLO with `/yolo`, or run with `--yolo`. Explicit deny rules still apply. The status bar shows `[yolo]` while it is on, and `/yolo` is stored with the session, so a resume comes back the same way. `--yolo` only sets the starting value for sessions you never toggled. Tools that never declare permission scopes are unaffected (they never prompted).

To start in YOLO mode every time:

```lua
-- ~/.config/maki/init.lua
maki.setup({
    always_yolo = true,
})
```

## Bash Command Parsing

Bash commands get parsed with tree-sitter to extract individual commands. Something like `cd /tmp && cargo test` is checked as two separate commands.

Some constructs are too complex to analyze statically, so they always trigger a prompt:

- Command substitution: `$(...)`, backticks
- Process substitution: `<(...)`, `>(...)`
- Subshells: `(...)`
- Arithmetic expansion: `$((...))`

Brace groups `{ ... }` and control flow (`if`, `for`, …) are segmented when possible; they do not by themselves force a prompt the way substitutions do.

## Plugin Permissions

Lua plugins have a separate, unrelated gate. A `plugin.toml` manifest next to the Lua file controls which gated `maki.*` APIs it may call. No manifest means every gated call is denied, including for your own `init.lua`. The [Lua API reference](/docs/lua-api/#plugin-permissions) documents the manifest and lists every permission.

It runs after [folder trust](/docs/folder-trust/) has let the Lua file load, and
limits which APIs the file reaches rather than sandboxing the file.

## Network Addresses

`webfetch`, `websearch` and every plugin that calls `maki.net` go through one guard. A request to a private, loopback or link-local address is refused, and so is a redirect that lands on one. The model picks these URLs, so a page it reads could otherwise talk it into fetching `http://169.254.169.254/` or an admin panel on your LAN.

To reach a service on your own machine or network, list it in [`net.allowed_private_hosts`](/docs/configuration/#net). An allowed host also keeps plain `http://` instead of being upgraded to `https://`, since a service on your LAN rarely has a certificate.

## Maki's Own Files

The file tools and every plugin that calls `maki.fs` go through one guard. Maki's state, data, cache and log directories are closed, so provider tokens, session history and logs stay out of reach of `read`, `glob`, `grep` and the rest. Closing the directories covers whatever Maki stores there next.

A few paths inside them stay open, because features need them:

- Memory notes, the skill reference and plan files: read and write.
- Package checkouts: read only. Reading a package is how you review one, and Maki loads those files as Lua on the next start.

`glob` and `grep` prune the closed paths while walking, so a search started above them returns the same results as one started inside.

In a config directory, `.env`, `providers.toml` and `mcp.toml` are closed because they hold keys in plaintext. Maki reads them itself, so nothing stops working, and the refusal names the file for you to open. `permissions.toml` is readable and never writable, since it is where you write down what Maki may do. The rest of the directory stays reachable, `config.toml`, skills and Lua modules included. Whether the agent may write there is the permission layer's question, and it already prompts for paths outside the folder you opened. What a repository may tell Maki to do is [folder trust](/docs/folder-trust/).

All of this holds only while `bash` is gated. A shell command reads these files directly, and only an OS sandbox can stop that.

### Opening one of them

Sometimes you want the agent to read `<logs>/maki.log`, or a session file. Those refusals turn into a permission prompt: `read`, `glob` and `grep` ask for the path, and your answer opens it. An ordinary path never asks, so reads stay free.

An approval holds for the session. Answering "always" writes it to your own `permissions.toml`, under one reserved section:

```toml
[maki_files]
read  = ["/home/you/.local/state/maki/logs"]
write = []
```

`read` makes the path readable. `write` makes it readable and writable. Both accept a file or a directory, and a directory covers everything under it.

Some paths never ask, and no entry opens them:

- The credentials: `auth/` in the state directory, and `.env`, `providers.toml` and `mcp.toml` in a config directory.
- `pack-approvals.json`, which records the packages you let Maki run as code.
- `permissions.toml`, for writes. A policy the agent can edit is no policy.
- Package checkouts, for writes. Maki loads them as Lua on the next start.

An entry naming one of those fails the load and names the line, rather than sitting in the file looking effective. So does an entry for a path nothing refuses. A file with one bad line opens none of its paths.

Only your own config is read. A `[maki_files]` section in a repository's `.maki/permissions.toml` is ignored at every trust level, and `maki_files` is a reserved name that no tool may register under.

YOLO mode changes none of this. It skips prompts, so no answer is recorded, and the guard still refuses. A standing `allow` rule for `read` does the same. Where the prompt is skipped the refusal itself names the `[maki_files]` entry that would open the path, so the agent can tell you what to add.

## Session Persistence

When you save a session, its permission rules are saved too. Loading the session restores them.
