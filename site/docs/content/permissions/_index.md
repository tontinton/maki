+++
title = "Permissions"
weight = 6
[extra]
group = "Reference"
+++

# Permissions

Maki uses a permission system to decide what each tool is allowed to do and when to ask you first.

## Folder Trust

A project `.maki` directory can set environment variables, start MCP servers,
add permission rules, and run Lua inside the Maki process. None of it loads
until you trust the folder. The first interactive start in an untrusted project
prints the project root, names the files that project ships, and asks. The
default answer is no. A project with none of these files is never asked about.

| Gated file | What it can do |
|------------|----------------|
| `.maki/init.lua` | Runs Lua inside Maki at startup |
| `.maki/.env` | Sets environment variables for the session, including `BASE_URL` and API keys |
| `.maki/mcp.toml` | Starts MCP server processes |
| `.maki/permissions.toml` | Adds allow rules and defaults. Its deny rules apply without trust |

`.maki/config.toml` is not on the list because Maki no longer reads it. It logs
a warning when it finds one.

The answer covers one project root: the active Git checkout, or the working
directory outside Git. Linked worktrees decide separately and load
configuration from their own checkout. Starting Maki in your home directory
loads no project config at all, because `~/.maki` there is your global config.

Trust gates the code a project runs. The text a project puts in the prompt is
never gated, so these load at any trust level:

- `AGENTS.md` and the other instruction files
- Commands under `.maki/commands` and `.claude/commands`
- Skills under `.maki/skills`, `.claude/skills`, `.opencode/skills` and
  `.agents/skills`. Every project skill name and description is in the system
  prompt from startup

Read them the way you read the code you pulled. A repository can still try to
steer the agent through what the model reads, so trust is not a sandbox. The
permission prompt on each tool call is what limits it, at every trust level.

### Deny Rules Without Trust

An untrusted project's `.maki/permissions.toml` still contributes its `deny`
scopes. Its `allow` scopes are dropped, and so is any `default` it sets. A
repository can narrow what the agent may do inside it and can never widen it,
so a project can ship a deny list without asking anyone to trust the rest of
its configuration. Deny wins across every layer, so an untrusted project deny
still beats a global allow.

### Answers in an Untrusted Folder

Maki writes nothing into a folder you declined. The project answers in a
[permission prompt](#permission-prompts) still work, and they last until you
close the session instead of reaching `.maki/permissions.toml`. The prompt
labels them `Project (this session)` and `Deny project (this session)` and
points at `D`. For an answer that outlives the session use `A` or `D`, which go
to your own `~/.config/maki/permissions.toml`, or trust the folder first.

### Managing Trust

```bash
maki trust add [PATH]
maki trust add [PATH] --yes
maki trust remove [PATH]
maki trust list
```

`PATH` defaults to the current directory. `add` asks before it records anything
unless you pass `--yes`. `list` shows trusted and rejected folders, and `remove`
clears either kind of decision. None of these start the Lua host, so they are
safe to run in a folder you have not read yet.

Decisions are stored outside the project, and they follow the checkout rather
than a remote or a commit. Outside Git, moving a folder means answering again.

### What a Yes Covers

Your yes covers the kinds of gated file the folder had that day, since that is
what the question named. Maki records the names and not the contents, so Lua
that changes in a later pull runs under the answer you already gave. Run
`maki trust remove` on a project when that stops being what you want.

A project that adds a kind of file outside that set is asked about again, and
the question says which one arrived. Say no there and the folder becomes a
stored no. A file that comes and goes with the branch you have checked out keeps
its place, so switching branches does not cost you the answer.

### Folders You Already Used

Projects your earlier sessions ran in are trusted without a question, so
upgrading brings no new prompt in the repositories you work in every day. That
set is taken once and never grows, so a project whose first session comes after
the upgrade is asked about like any other folder, and a fresh install
grandfathers nothing. A folder you already rejected stays rejected.

The grant covers the checkout a session ran in and nothing above it, so a
session in `~/projects/myrepo/src` trusts `~/projects/myrepo` and leaves
`~/projects` alone. It covers the files the project ships that day like any
other answer, and the first start in such a project writes the decision down,
where `maki trust list` shows it and `maki trust remove` clears it.

### Non-Interactive Sessions

Headless runs, the SDK, ACP, and utility subcommands never ask. An untrusted
folder is skipped, the skipped path is reported on standard error, and the
session continues on global configuration. There is no environment variable
override. Containers, CI jobs, and editor integrations should trust the folder
up front:

```bash
maki trust add --yes .
```

Use `--yes` only for a folder you reviewed and mean to trust.

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
[folder trust](#folder-trust).

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

Project files **cannot** set `default = "allow"` (top-level, per-tool, or
MCP). That value is ignored so a repository cannot grant itself full access.
Project **allow lists** work once the folder is trusted. Put
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

The keys are the same in an [untrusted folder](#answers-in-an-untrusted-folder),
where `a` and `d` last for the session.

ACP clients offer four options set by the protocol. "Allow always" lasts for the
session, and "Reject always" is a project answer that follows folder trust the
same way as the TUI. In an untrusted folder that option reads "Reject for this
session", because that is all it can do there.

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

This gate runs after [folder trust](#folder-trust) has let the Lua file load,
and it does not sandbox that file.

## Network Addresses

`webfetch`, `websearch` and every plugin that calls `maki.net` go through one guard. A request to a private, loopback or link-local address is refused, and so is a redirect that lands on one. The model picks these URLs, so a page it reads could otherwise talk it into fetching `http://169.254.169.254/` or an admin panel on your LAN.

To reach a service on your own machine or network, list it in [`net.allowed_private_hosts`](/docs/configuration/#net). An allowed host also keeps plain `http://` instead of being upgraded to `https://`, since a service on your LAN rarely has a certificate.

## Session Persistence

When you save a session, its permission rules are saved too. Loading the session restores them.
