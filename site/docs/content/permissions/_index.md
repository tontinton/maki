+++
title = "Permissions"
weight = 8
[extra]
group = "Reference"
+++

# Permissions

Maki uses a permission system to decide what each tool is allowed to do and when to ask you first.

Rules come from three layers, combined for resolution:

1. **Session rules**, set during the current session (in-memory only)
2. **Config rules**, loaded from TOML permission files
3. **Builtin rules**, the hardcoded defaults

Any matching deny blocks the tool. No exceptions.

## Check Flow

For every tool call, Maki resolves permission like this:

1. **Deny wins**: if any rule from any layer matches the tool and scope with a deny, the call is blocked immediately.
2. If **YOLO** is active and no deny matched, allowed.
3. **Plan file auto-allow**: file write tools targeting the plan file path are allowed automatically (only if no deny rule matched in step 1). In plan mode, writes to any other path are rejected before this step, and MCP tools are blocked entirely.
4. Fall back to `default` (per-tool, then global). Built-in default is `"prompt"`.

## Builtin Defaults

File-write tools are pre-allowed inside the project working directory (cwd at session start, canonicalized). Paths outside that tree still need a prompt or an explicit allow rule:

| Tool | Scope | Notes |
|------|-------|-------|
| `write` | `<cwd>/**` | Outside cwd requires permission |
| `edit` | `<cwd>/**` | Outside cwd requires permission |
| `multiedit` | `<cwd>/**` | Outside cwd requires permission |
| `edit_lines` | `<cwd>/**` | Same, when the opt-in tool is enabled |
| `insert_lines` | `<cwd>/**` | Same, when the opt-in tool is enabled |
| `task` | `*` | Subagent spawning always allowed |

These tools have no builtin allow rule, so they prompt (or follow your `default`) every time unless you add rules:

- `bash` - Shell commands (scopes come from tree-sitter parsing)
- `websearch` - Web search queries
- `webfetch` - URL fetching

Tools that never declare permission scopes (for example `read`, `glob`, `grep`, `index`, `memory`, `skill`, `todo_write`) **skip** the permission manager entirely. They always run. If you need to block one of them, turn the plugin off in `init.lua` (`plugins.read = { enabled = false }`) rather than using `permissions.toml`.

Container tools like `batch` and `code_execution` prompt for each inner tool individually.

## TOML Configuration

There are two permission files:

- **Global**: `~/.config/maki/permissions.toml`
- **Project**: `.maki/permissions.toml` (takes precedence over global)

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

Project files **cannot** set `default = "allow"` (top-level, per-tool, or MCP). That value is ignored so a project cannot grant itself full access. Project **allow lists** still work. Put `default = "allow"` only in the global file.

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
| `d` | Deny always for this project (confirm) |
| `D` | Deny always globally (confirm) |

Session and always-allow / always-deny choices need a second key (`Enter` or `y`) so a fat-finger does not rewrite your rules. Deny-once with `n` lets you type a short reason the agent will see.

### Scope Generalization

When you pick "always allow" (or always deny for MCP), the saved scope is generalized so it stays useful beyond that one call:

- **bash**: `cargo test --all` becomes `cargo *`
- **write / edit / multiedit / edit_lines / insert_lines**: `/path/to/file.rs` becomes `/path/to/**`
- **MCP tools**: always `*` (per-tool, so allowing `deepwiki.search` will not cover `deepwiki.fetch`)
- **webfetch / websearch** (and anything else gated): the exact URL or query string is stored as-is

For MCP tools, both allow and deny decisions generalize to `*` (the entire tool). MCP inputs are opaque JSON with no meaningful scope pattern. Denying a single MCP invocation denies that tool until you revoke the rule.

## YOLO Mode

To skip prompts on gated tools, toggle YOLO with `/yolo`, or run with `--yolo`. Explicit deny rules still apply. Tools that never declare permission scopes are unaffected (they never prompted).

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

## Session Persistence

When you save a session, its permission rules are saved too. Loading the session restores them.
