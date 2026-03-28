+++
title = "MCP"
weight = 6
+++

# MCP (Model Context Protocol)

Maki connects to external tool servers over MCP. Both **stdio** and **HTTP** transports are supported.

## Configuration

Add servers under `[mcp.*]` in your config:

- **Global**: `~/.config/maki/config.toml`
- **Project**: `.maki/config.toml` (project config wins when both set a value)

### Stdio Transport

```toml
[mcp.filesystem]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.github]
command = ["gh", "mcp-server"]
environment = { GITHUB_TOKEN = "ghp_xxxx" }
timeout = 10000
enabled = false
```

### HTTP Transport

```toml
[mcp.analytics]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer tok123" }
```

## Server Options

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `command` | array | — | Stdio: program and arguments |
| `url` | string | — | HTTP: server URL (http/https) |
| `environment` | map | — | Stdio: environment variables |
| `headers` | map | — | HTTP: request headers |
| `timeout` | u64 | 30000 | Request timeout in milliseconds (1-300000) |
| `enabled` | bool | true | Whether the server is active |

Pick one: `command` makes it a stdio server, `url` makes it HTTP.

## Server Names

Names must be ASCII alphanumeric (hyphens allowed). Double underscores (`__`) are reserved — they separate server and tool names internally. Names cannot collide with built-in tools.

## Tool Namespacing

Tools are prefixed with their server name: `{server}__{tool}`. A `read` tool on the `filesystem` server becomes `filesystem__read`, avoiding conflicts with other servers and built-in tools.

## Runtime Toggling

Servers can be toggled on or off at runtime via the MCP picker in the UI. The state is saved back to your config file.

## Server Status

| Status | Meaning |
|--------|---------|
| Connecting | Config looks good, waiting for the server to start |
| Running | Up and running, tools are available |
| Disabled | Disabled in config or toggled off at runtime |
| Failed | Startup failed; the error is shown in the UI |

If one server fails, the rest still start normally.

## Startup Flow

1. Load and merge global + project config
2. Validate server names and settings
3. Start all enabled servers in parallel
4. Collect tool lists from each running server
5. Namespace the tools and register them with the agent

## Shutdown

All transports shut down in parallel. For HTTP, Maki sends a DELETE request with the session ID to clean up.
