An AI coding agent optimized for cost and token efficiency.

## MCP Servers

Configure MCP servers in `~/.config/maki/config.toml` (global) or `.maki/config.toml` (project, takes precedence).

### Local (stdio)

```toml
[mcp.filesystem]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.github]
command = ["gh", "mcp-server"]
environment = { GITHUB_TOKEN = "ghp_xxxx" }
```

### Remote (HTTP)

```toml
[mcp.analytics]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer tok123" }
```

### Options

All servers support `timeout = 30000` (ms, max 300000) and `enabled = true/false`.

Server names must be alphanumeric + hyphens and not collide with built-in tools. Tools are exposed as `<server>__<tool>`.
