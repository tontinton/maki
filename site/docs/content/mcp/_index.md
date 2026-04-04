+++
title = "MCP"
weight = 6
[extra]
group = "Reference"
+++

# MCP (Model Context Protocol)

Maki connects to external tool servers over MCP. Both **stdio** and **HTTP** transports are supported.

## Configuration

Add servers under `[mcp.*]` in your config:

- **Global**: `~/.config/maki/config.toml`
- **Project**: `.maki/config.toml` (project config wins when both set a value)

### Stdio

```toml
[mcp.filesystem]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.github]
command = ["gh", "mcp-server"]
environment = { GITHUB_TOKEN = "ghp_xxxx" }
timeout = 10000
enabled = false
```

### HTTP

```toml
[mcp.analytics]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer tok123" }
```

### All options

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `command` | array | | Stdio: program + args |
| `url` | string | | HTTP: server URL |
| `environment` | map | | Stdio only |
| `headers` | map | | HTTP only |
| `timeout` | u64 | 30000 | Milliseconds (1-300000) |
| `enabled` | bool | true | |

Set `command` for stdio, `url` for HTTP. Pick one.

## Naming and namespacing

Server names are ASCII alphanumeric, hyphens ok. Tools get prefixed with their server name: a `read` tool on the `filesystem` server becomes `filesystem__read`. Because of this, `__` is reserved and names can't collide with built-in tools.

## Runtime toggling

Turn servers on/off from the MCP picker in the UI. Changes save back to your config.

## Status

| Status | Meaning |
|--------|---------|
| Connecting | Waiting for the server to come up |
| Running | Tools available |
| Disabled | Off in config or toggled off in UI |
| Failed | Error shown in UI |
| NeedsAuth | Waiting for OAuth (see below) |

If one server fails, the rest still work.

## OAuth

Some HTTP servers need auth. When that happens, Maki opens your browser to log in. Other servers keep working while you authenticate. Tokens refresh on their own. If you change the server URL, you log in again.

```bash
maki mcp auth <server-name>     # manually trigger auth
maki mcp logout <server-name>   # remove stored tokens
```

## Prompts

MCP servers can expose prompts (reusable message templates). Maki shows them as slash commands in the command palette: `/server:prompt-name`. Type `/` to filter.

```
/github:create-pr           # no arguments
/analytics:report monthly   # one argument
/review:code src tests      # multiple, positional
```

Skip a required argument and Maki shows a usage hint. Prompts are fetched at startup and on reconnect, so new ones need a restart. Only text content is supported.
