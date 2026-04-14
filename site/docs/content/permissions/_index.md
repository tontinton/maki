+++
title = "Permissions"
weight = 4
[extra]
group = "Reference"
+++

# Permissions

Maki uses a permission system to decide what each tool is allowed to do and when to ask you first.

Rules come from three layers, checked in this order:

1. **Session rules**, set during the current session (in-memory only)
2. **Config rules**, loaded from TOML permission files
3. **Builtin rules**, the hardcoded defaults

First match wins.

## Check Flow

For every tool call, Maki resolves permission like this:

1. If any **deny** rule matches, denied. Full stop.
2. If **YOLO** is active, allowed.
3. If any **allow** rule matches all scopes, allowed.
4. Otherwise, prompt the user.

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
allow_all = false

[bash]
allow = [
    "cargo *",
    "git *",
]
deny = [
    "rm -rf *",
    "sudo *",
]

[write]
deny = ["/etc/*"]
```

Each tool gets its own section with `allow` and `deny` arrays. Values are glob-like scope patterns, or `true` to match everything.

## Scope Patterns

| Pattern | Matches |
|---------|--------|
| `*` | Any single value |
| `**` | Everything |
| `prefix*` | Values starting with prefix |
| `dir/**` | `dir` itself or anything under it |
| `exact` | Exact match only |

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
- **webfetch/websearch**: always `*`

Deny rules are saved with the exact scope. You denied something specific, so it stays specific.

## YOLO Mode

To skip all prompts, toggle YOLO with the `/yolo` command, or run with `--yolo`. Explicit deny rules still apply.

To start in YOLO mode every time:

```toml
# ~/.config/maki/config.toml
always_yolo = true
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
