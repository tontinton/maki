+++
title = "MCP"
weight = 7
[extra]
group = "Reference"
+++

# MCP (Model Context Protocol)

Maki connects to external tool servers over MCP. Both **stdio** and **HTTP** transports are supported.

## Configuration

Add servers under `[mcp.*]` in your MCP config:

- **Global**: `~/.config/maki/mcp.toml`
- **Project**: `.maki/mcp.toml` in the active Git checkout, or in the working
  directory outside Git (project config wins when both set a value)

Servers in the project file start only after you trust that folder, see
[Folder Trust](/docs/folder-trust/).

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
headers = { Authorization = "Bearer ${ANALYTICS_TOKEN}" }
```

`headers` and `environment` values expand `${VAR}` from the environment. A
referenced variable that is unset or empty fails that server with the variable
named in its status, instead of sending a dangling `Bearer ` and getting a 401.

Maki does not pass provider API keys such as `ANTHROPIC_API_KEY` to the
processes it starts, so a stdio server that needs one gets it through
`environment = { ANTHROPIC_API_KEY = "${ANTHROPIC_API_KEY}" }`.

Some HTTP servers need OAuth but have no dynamic client registration. For those, give Maki a static client:

```toml
[mcp.acme]
url = "https://mcp.acme.example.com/mcp"
oauth = { client_id = "acme-client", client_secret = "s3cret", callback_port = 3118, callback_path = "/callback", callback_hostname = "localhost" }
```

For a server whose certificate comes from a private CA, point `ca_file` at a PEM bundle:

```toml
[mcp.internal]
url = "https://mcp.corp.example.com/mcp"
ca_file = "~/certs/corp-ca.pem"
```

Maki checks certificates against the operating system's trust store, so a CA that is already trusted in the macOS keychain or the Windows certificate store works without `ca_file`. Set `ca_file` for a CA that is not in that store. The bundle replaces the default CAs for this server and its OAuth endpoints. If OAuth goes through a public provider, add the public CAs to the bundle too, for example `cat /etc/ssl/cert.pem corp-ca.pem > bundle.pem`. The path expands `${VAR}` and `~`, and a relative path starts from the folder of the `mcp.toml` that sets it. On macOS and Linux, you can instead trust a CA for every HTTPS request Maki makes by pointing `SSL_CERT_FILE` at a bundle that holds the public CAs as well.

### All options

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `command` | array | | Stdio: program + args |
| `url` | string | | HTTP: server URL |
| `environment` | map | | Stdio only. Values expand `${VAR}` from the environment |
| `headers` | map | | HTTP only. Values expand `${VAR}` from the environment |
| `oauth` | table | | HTTP only: OAuth client (optional `client_id`, optional `client_secret`, optional `callback_port`, optional `callback_path`, optional `callback_hostname`, optional `callback_scheme`) |
| `ca_file` | path | | HTTP only. PEM bundle that replaces the default CAs for this server and its OAuth |
| `timeout` | u64 | 30000 | Milliseconds (1-300000) |
| `enabled` | bool | true | |
| `always_load` | bool | false | Skip tool search, load all tools upfront |

Set `command` for stdio, `url` for HTTP. Pick one.

One option lives at the top level of `mcp.toml`, outside any server:

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `defer_tools` | usize | 10 | Defer tools only when more than this many exist. Ignored with [native tool search](#loads-and-the-prompt-cache), which always defers |

## Tool search

Every tool definition a server exposes costs context window space, on every request. Take Datadog's MCP server: with all toolsets on it ships over 100 tools, when a task often needs three.

So Maki, like Claude Code, defers MCP tools by default. The model sees one small `tool_search` tool that lists the deferred names, searches when it actually needs something, and the matches stay loaded for the rest of the session. Resume a session and the tools it was using come back. Subagents keep their own loads, so their searches don't bloat your main conversation.

```
server ships 117 tool definitions
        │
  native tool search, or more than defer_tools (10)?
   │ no          │ yes
   ▼             ▼
   all load      context gets one small tool: tool_search
   upfront       │
                 │  model: tool_search("logs")
                 ▼
                 3 matches load, stay for the session
                 114 definitions never enter context
```

You don't configure anything for this. Add the server as usual:

```toml
[mcp.datadog]
url = "https://mcp.datadoghq.com/api/unstable/mcp-server/mcp?toolsets=all"
```

Ask about an incident, and the model searches for something like `datadog logs`, gets back the few matching tools, and the other hundred definitions never enter the conversation.

### Loads and the prompt cache

Tool definitions sit at the front of the prompt, inside the cached prefix. Adding one there changes the prefix, so the next request rewrites the whole conversation at cache-write rates. On a long session that single rewrite can cost more than the search saved.

With native tool search a load never touches the tools array. That is the Anthropic API (direct and Bedrock) on Claude Haiku 4.5, Sonnet 4.5, Opus 4.5 and newer. Older Claude models have no tool search and work like other providers, and so does a model Maki does not know yet. Maki sends every deferred definition on every request, marked as deferred so it stays out of the context, and a search result points at its matches, which the API expands in place. Calling a deferred tool straight from the catalog loads it the same way. The array is the same bytes all session long, and a load costs only the few hundred tokens of the search result.

```
request N     tools: [read, edit, ..., 117 deferred, tool_search]   cache hit
              model: tool_search("logs")
result        3 matches, expanded by the API
request N+1   tools: same bytes                                      cache hit
```

So with native tool search Maki always defers, whatever the count, and a server that connects mid-session costs no rebuild. That includes an `always_load` server that connects late: its tools join deferred, and the model finds them through `tool_search`. A server that disconnects keeps its entries, and a call to one of its tools tells the model the server is not connected.

Other providers have no such mechanism, so a load adds the definition to the tools array and the cache is rebuilt once. A gateway speaking the Anthropic protocol may or may not pass the expansion through. So a custom provider, or the built-in `anthropic` pointed at another `base_url`, gets the rebuild until its `providers.toml` row sets `supports_deferred_tools = true` (see [Provider fields](../providers/#provider-fields)).

Without native tool search, with 10 or fewer tools across all your servers there is no search step: at that size, searching costs more than it saves, so everything loads upfront. If servers that connect later push the count past that line, `tool_search` joins then, with their tools behind it. The top-level `defer_tools` key moves that line:

```toml
defer_tools = 30

[mcp.github]
url = "https://api.githubcopilot.com/mcp/"
```

Set it to 0 to always defer, or above your tool count to never defer.

If one server should skip the search step entirely, opt it out:

```toml
[mcp.linear]
command = ["linear-mcp-server"]
always_load = true
```

Good for small servers you rely on every turn. On a big server it defeats the point: every definition is back in your context on every request.

## Naming and namespacing

Server names are ASCII alphanumeric, hyphens ok (no dots). Tools get prefixed with their server name: a `read` tool on the `filesystem` server becomes `filesystem__read`. Because of this, `__` is reserved and names can't collide with built-in tools.

Permission rules for MCP tools use the same nested form under `[mcp.<server>]` in `permissions.toml`. See [Permissions](/docs/permissions/#mcp-tool-permissions).

## Runtime toggling

Open the MCP picker with `/mcp`. Turn servers on or off there; changes save back to your config (project or global, depending on which file defined the server).

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

Servers without dynamic client registration need a client you registered yourself (e.g. your own app on their platform). Add it to the server config so the auth flow uses it instead of trying to register. Without `client_id`, the `callback_*` fields still apply and Maki registers a client dynamically with that redirect URI:

| Field | Type | Notes |
|-------|------|-------|
| `client_id` | string | Optional, client id of your registered app |
| `client_secret` | string | Optional, for confidential clients (needs `client_id`) |
| `callback_port` | u16 | Optional, pins the loopback port so the redirect URI can be pre-registered |
| `callback_path` | string | Optional, loopback path of the redirect URI (default `/mcp/oauth/callback`) |
| `callback_hostname` | string | Optional, loopback hostname of the redirect URI (default `127.0.0.1`) |
| `callback_scheme` | string | Optional, `http` (default) or `https` |

Set `callback_port` when the server only accepts exact redirect URIs. Otherwise Maki falls back to its default port, then to any free port, so the redirect URI changes between runs. Set `callback_path` when the server registered a different path (e.g. `/callback`). Set `callback_hostname` to `localhost` when the server registered the name form instead of the IP (the listener still binds to 127.0.0.1).

Set `callback_scheme = "https"` (needs `callback_port`) when the server only accepts https redirect URIs, such as `https://localhost:3119/callback`. Maki runs no https listener, so the login must run in `maki mcp auth <server-name>` (the TUI can't finish it). When the browser shows a connection error, paste the full URL from its address bar into the terminal.

### Headless machines

On a machine without a browser (say, a dev server over SSH), run `maki mcp auth <server-name>`. Maki prints the login URL. Open it on your laptop and log in. The browser lands on a `http://127.0.0.1:19876/...` page that fails to load. Copy that full URL from the address bar and paste it into the terminal to finish the login.

## Prompts

MCP servers can expose prompts (reusable message templates). Maki shows them as slash commands in the command palette: `/server:prompt-name`. Type `/` to filter.

```
/github:create-pr           # no arguments
/analytics:report monthly   # one argument
/review:code src tests      # multiple, positional
```

Skip a required argument and Maki shows a usage hint. Prompts are fetched at startup and on reconnect, so new ones need a restart. Only text content is supported.
