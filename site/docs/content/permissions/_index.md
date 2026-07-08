+++
title = "Permissions"
weight = 4
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
3. **Plan file auto-allow**: file write tools targeting the plan file path are allowed automatically (only if no deny rule matched in step 1).
4. Fall back to `default` (per-tool, then global). Built-in default is `"prompt"`.

## Builtin Defaults

| Tool | Scope | Notes |
|------|-------|-------|
| `write` | Project directory | Files outside require permission |
| `edit` | Project directory | Files outside require permission |
| `multiedit` | Project directory | Files outside require permission |
| `task` | `*` (all) | Subagent spawning always allowed |

These tools require explicit permission:

- `bash` - Shell commands
- `websearch` - Web search queries
- `webfetch` - URL fetching

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

Note: `default = "allow"` only works in the global file. Projects cannot grant themselves full access.

## Scope Patterns

| Pattern | Matches |
|---------|--------|
| `*` | Any single value |
| `**` | Everything |
| `prefix*` | Values starting with prefix |
| `dir/**` | `dir` itself or anything under it |
| `exact` | Exact match only |

## MCP Tool Permissions

MCP tools use natural TOML nesting. Server names are table keys under `[mcp]`, tool names are array values:

```toml
[mcp.deepwiki]
allow = ["search", "fetch"]    # allow these tools

[mcp.github]
deny = ["admin_delete"]         # deny this tool

[mcp.lean-lsp]
default = "allow"               # allow all tools on this server
```

Tool names must match `^[a-zA-Z0-9_-]{1,64}$` (no dots, max 64 chars). Server names cannot contain dots.

## Permission Prompts

When a tool needs permission, Maki asks you. Here are the keys:

| Key | Action |
|-----|--------|
| `y` | Allow once |
| `s` | Allow for this session |
| `a` | Always allow (project, saved to `.maki/permissions.toml`) |
| `A` | Always allow (global, saved to `~/.config/maki/permissions.toml`) |
| `n` | Deny once |
| `d` | Deny always (project) |
| `D` | Deny always (global) |

### Scope Generalization

When you pick "always allow", the saved scope is generalized so it stays useful beyond just that one command:

- **bash**: `cargo test --all` becomes `cargo *`
- **write/edit/multiedit**: `/path/to/file.rs` becomes `/path/to/**`
- **MCP tools**: always `*` (per-tool, so allowing `deepwiki.search` won't cover `deepwiki.fetch`)
- **webfetch/websearch**: always `*`

For MCP tools, both allow and deny decisions generalize to `*` (the entire tool). This is because MCP tool inputs are opaque JSON with no meaningful scope pattern to differentiate. Denying a single MCP invocation denies the tool entirely until you revoke the rule.

## YOLO Mode

To skip all prompts, toggle YOLO with the `/yolo` command, or run with `--yolo`. Explicit deny rules still apply.

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
- Subshells: `(...)`, `{ ... }`
- Arithmetic expansion: `$((...))`

## Session Persistence

When you save a session, its permission rules are saved too. Loading the session restores them.
