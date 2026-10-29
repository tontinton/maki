+++
title = "Lua API"
weight = 10
[extra]
group = "Reference"
+++

# Lua API

Maki plugins are plain Lua files. Everything a plugin can touch lives under
one global table: `maki`. This reference documents every module, function,
and method. It is generated straight from the source code by `maki-docgen`.
For where plugin files live and how to load them, read the
[Plugins guide](/docs/plugins/) first.

The API tries to mirror Neovim as much as possible (`maki.fs`, `maki.uv`,
`maki.treesitter`, `maki.keymap`, `maki.base64`), signatures are kept identical
so code can be copy-pasted between the two without too many modifications.

Plugins run compiled to native code (Luau JIT). If you are debugging a
plugin and want full backtraces, start maki with `--no-jit`: it runs your
Lua on the interpreter with complete debug info instead.

A small plugin looks like this:

```lua
maki.api.register_command({
  name = "greet",
  description = "Say hello from Lua",
  handler = function()
    maki.ui.flash("hello from a plugin!")
  end,
})
```

## How to read this reference

Signatures use Neovim notation: `{path}` is a required argument, `{opts?}`
is optional, and `{...}` is variadic.

One convention to remember: fallible runtime operations return a
`(value, err)` pair instead of throwing. Check `err` before using `value`:

```lua
local text, err = maki.fs.read("config.json")
if err then
  maki.log.error("read failed: " .. err)
  return
end
```

Lua errors are reserved for programmer mistakes, like passing a number where
a string belongs.

## Permissions and plugin.toml {#plugin-permissions}

Sensitive APIs are gated per plugin file, and every gated function's entry in
this reference names the permission it needs. A gated call without its
permission raises `permission denied: '<name>' not granted for this plugin`.
Inside an action of your plugin, a call also raises when the `capability`
of the action leaves its permission out, see
`maki.api.register_action`.

- `fs_read`: reading files, and locating the directories maki keeps them in
- `fs_write`: creating, changing, and removing files
- `net`: outbound network requests
- `run`: starting processes
- `env`: reading the process environment, where secrets live
- `agent`: starting model turns: prompting a chat, or running its own sessions
- `session_read`: reading conversation content: session history, the chat input draft, and the content fields of events
- `session_write`: adding to, removing, or creating conversation content: new sessions, prompts, notices, and deleting sessions or task transcripts
- `session_settings`: changing how a session runs without touching its content: mode, title, model, thinking, fast mode, working directory, and workflow
- `input`: changing the chat input, which is what your next Enter sends

Grants come from a `plugin.toml` next to the Lua file (for
`~/.config/maki/init.lua` that is `~/.config/maki/plugin.toml`):

```toml
min_maki_version = "0.4.12"

[permissions]
fs_read = true
fs_write = true
net = true
run = true
env = true
agent = true
session_read = true
session_write = true
session_settings = true
input = true
```

The rules:

- No `plugin.toml` at all: every permission is denied, and maki logs a
  warning at load time.
- `plugin.toml` exists: permissions default to granted; set a key to
  `false` to revoke it. An empty file grants everything.
- Invalid TOML: everything denied, with a warning in the log.
- A package, or a plugin maki ships, is read the other way round: a key it
  does not name is not requested, so its `plugin.toml` lists everything it
  uses. Only a `plugin.toml` you wrote yourself defaults to granted.
- `net_hosts` narrows `net = true` to a host allowlist, such as
  `["api.acme.com", "*.acme.dev"]`. Without it, `net` reaches any public
  host. A plugin that registers a provider must set it. See
  [Plugin egress](/docs/permissions/#plugin-egress-net-hosts).
- `min_maki_version` is optional and takes a plain semantic version as a lower
  bound, so ranges do not work. When the field is invalid or the running
  version is older, Maki skips the Lua in that directory and warns at startup
  instead of failing. The same floor applies to an installed package, which is
  skipped while the rest keep loading. `--no-plugins` still skips every user
  plugin at once.

## Overview

| Module | What it is for |
| --- | --- |
| [`maki`](#maki) | The global entry point. |
| [`maki.pack`](#maki-pack) | Declare global packages and inspect package state. |
| [`maki.api`](#maki-api) | Plugin registration. |
| [`maki.plan`](#maki-plan) | Plan-mode surface for plugins. |
| [`maki.agent`](#maki-agent) | Subagent primitives for plugins that need to talk to an LLM. |
| [`maki.agent.Session`](#maki-agent-Session) | A subagent session with its own conversation history. |
| [`maki.async`](#maki-async) | Tools for running things concurrently in Lua plugins. |
| [`maki.async.Semaphore`](#maki-async-Semaphore) | A counting semaphore for limiting how many tasks run at once. |
| [`maki.async.Permit`](#maki-async-Permit) | One slot in a semaphore, obtained from `Semaphore:acquire()`. |
| [`maki.async.Task`](#maki-async-Task) | Handle returned by `maki.async.spawn`. |
| [`maki.base64`](#maki-base64) | Base64 encoding and decoding, modelled after `vim.base64`. |
| [`maki.env`](#maki-env) | Paths to maki's own directories (config, state, logs, legacy). |
| [`maki.fn`](#maki-fn) | Process and environment helpers, modeled after Neovim's `vim.fn` job |
| [`maki.fs`](#maki-fs) | File-system utilities, modelled after `vim.fs` and `vim.uv`. |
| [`maki.image`](#maki-image) | Small building blocks for working with images: probe metadata, decode |
| [`maki.image.Image`](#maki-image-Image) | An image you can inspect, resize, and re-encode. |
| [`maki.interpreter`](#maki-interpreter) | Run Python code in a memory-safe, time-limited sandbox. |
| [`maki.json`](#maki-json) | JSON encoding, decoding, and schema validation. |
| [`maki.json.SchemaValidator`](#maki-json-SchemaValidator) | A compiled JSON Schema validator. |
| [`maki.keymap`](#maki-keymap) | Key mappings, modeled after `vim.keymap`. |
| [`maki.log`](#maki-log) | Structured logging for plugins. |
| [`maki.model`](#maki-model) | The model behind the focused session. |
| [`maki.net`](#maki-net) | HTTP and plain TCP for plugins. |
| [`maki.net.Conn`](#maki-net-Conn) | A TCP connection opened by `maki.net.connect`. |
| [`maki.provider`](#maki-provider) | Providers implemented in Lua. |
| [`maki.provider.auth`](#maki-provider-auth) | Credential storage for the providers this plugin registered. |
| [`maki.session`](#maki-session) | Host session primitives. |
| [`maki.Timer`](#maki-Timer) | Handle returned by `maki.defer_fn`. |
| [`maki.task`](#maki-task) | The subagents of the focused session and their transcripts. |
| [`maki.text`](#maki-text) | Text utilities: format conversion and the fuzzy matcher the built-in |
| [`maki.treesitter`](#maki-treesitter) | Tree-sitter parsing and query API. |
| [`maki.treesitter.language`](#maki-treesitter-language) | Language registry for tree-sitter grammars. |
| [`maki.treesitter.query`](#maki-treesitter-query) | Query compilation and lookup. |
| [`maki.treesitter.Query`](#maki-treesitter-Query) | A compiled tree-sitter query. |
| [`maki.treesitter.Tree`](#maki-treesitter-Tree) | A parsed syntax tree. |
| [`maki.treesitter.Node`](#maki-treesitter-Node) | A single node in a parsed syntax tree. |
| [`maki.treesitter.LanguageTree`](#maki-treesitter-LanguageTree) | Manages parsing of a source string for a single language. |
| [`maki.ui`](#maki-ui) | Functions for building interactive UI. |
| [`maki.ui.Win`](#maki-ui-Win) | Handle to a floating or split window. |
| [`maki.ui.Buf`](#maki-ui-Buf) | A content buffer that holds styled lines of text. |
| [`maki.uv`](#maki-uv) | System and environment utilities, modelled after `vim.uv`. |
| [`maki.yaml`](#maki-yaml) | YAML encoding and decoding. |

## maki {#maki}

The global entry point. Every API lives under this table.

---

### `maki.setup()` {#maki-setup}

```lua
maki.setup({config})
```

Apply your personal configuration. This is only available inside `init.lua` (not in plugins) and can be called at most once. The table accepts the same keys as the Configuration reference.

**Parameters:**

- `{config}` (`table`) Configuration table.

**Example:**

```lua
maki.setup({
model = "opus",
keymaps = false,
})
```

---

### `maki.split()` {#maki-split}

```lua
maki.split({s}, {sep}, {opts?})
```

Split {s} at each occurrence of {sep} and return the pieces as a
list. Mirrors Neovim's `vim.split`, so code using it can be copied
between Neovim and maki. {sep} is a Lua pattern unless `plain` is
set; an empty {sep} splits into single characters.

**Parameters:**

- `{s}` (`string`) String to split.
- `{sep}` (`string`) Separator: a Lua pattern, or literal text with `plain`.
- `{opts?}` (`table?`) Optional settings:
  - `plain` (`boolean?`) treat {sep} as literal text instead of a pattern.
  - `trimempty` (`boolean?`) drop empty pieces from the start and end of the result.

**Returns:** (`table`) List of split pieces.

**Example:**

```lua
maki.split("a,b,c", ",")                   -- { "a", "b", "c" }
maki.split("x*y*z", "*", { plain = true }) -- { "x", "y", "z" }
maki.split("\nhello\nworld\n", "\n", { trimempty = true }) -- { "hello", "world" }
```

---

### `maki.packadd()` {#maki-packadd}

```lua
maki.packadd({name})
```

Load an installed package that is not active.

**Parameters:**

- `{name}` (`string`) Package name.

---

### `maki.defer_fn()` {#maki-defer_fn}

```lua
maki.defer_fn({callback}, {ms})
```

Run {callback} once after {ms} milliseconds. It fires even if the tool
call that scheduled it has ended or was cancelled, which is what a
self-dismissing toast needs. For repeating work, use a
`maki.async.sleep` loop inside `maki.async.spawn`.

You get back a handle. Its `:stop()` cancels a callback that has not
fired yet, which is how you debounce: schedule, then stop and
reschedule on every new event. An error raised by the callback is
logged and dropped, since nobody is waiting for a result.

**Parameters:**

- `{callback}` (`function`) Called with no arguments.
- `{ms}` (`integer`) Delay in milliseconds. Zero fires on the next tick.

**Returns:** ([`maki.Timer`](#maki-Timer)) Handle with `:stop()` to cancel before it fires.

**Example:**

```lua
-- A toast that dismisses itself 4 seconds later:
local buf = maki.ui.buf({ scratch = true })
buf:line("copied!")
local win = maki.ui.open_win(buf, { split = "right", width = 20, height = 3 })
maki.defer_fn(function() win:close() end, 4000)

-- Repaint only after the user has stopped typing for half a second:
local pending
local function repaint_soon()
  if pending then
    pending:stop()
  end
  pending = maki.defer_fn(repaint, 500)
end
```

---

### `maki.notify()` {#maki-notify}

```lua
maki.notify({msg}, {level?}, {opts?})
```

Show a one line notice. By default it goes to `maki.ui.flash`, with
`{opts.title}` in front of the message when you pass one. Without a UI
(`maki -p`, the sdk, ACP), the notice goes to the log.

There is one handler for the whole process. Once a plugin calls
`maki.set_notify_handler`, notices from every plugin go through it.
That is how a UI plugin turns flashes into stacked toasts without
any of the callers knowing about it.

{level} reaches the handler untouched, and the default ignores it.

**Parameters:**

- `{msg}` (`string`) Notice text.
- `{level?}` (`string?`) Optional. Severity name such as "info", "warn" or "error".
- `{opts?}` (`table?`) Optional. `title` (string) labels the notice. Free form otherwise.

**Example:**

```lua
maki.notify("saved!")
maki.notify("build failed", "error", { title = "make" })
```

---

### `maki.set_notify_handler()` {#maki-set_notify_handler}

```lua
maki.set_notify_handler({handler})
```

Install the handler that every `maki.notify` call in the process goes
through, in place of the default flash. Pass `nil` to put the default
back.

The handler runs on the Lua thread, so keep it short and hand real
work to `maki.async.run`. If it raises an error, the error is logged
and the notice falls back to `maki.ui.flash`, so the user still sees
it. Unloading the plugin that installed the handler also restores the
default.

**Parameters:**

- `{handler}` (`function|nil`) Handler `function(msg, level?, opts?)`, or nil.

**Example:**

```lua
local Toast = require("maki.toast")
maki.set_notify_handler(function(msg, level, opts)
  Toast.show(msg, { title = opts and opts.title, level = level })
end)
```

---

### `maki.deepcopy()` {#maki-deepcopy}

```lua
maki.deepcopy({orig}, {noref?})
```

Copy {orig} and every table inside it, keeping each table's metatable.
That is what lets a copied empty array still encode as `[]` instead of
`{}`. Use it instead of a hand-rolled copy for anything you pass back to
maki. Table keys are copied too. Functions, userdata (including
`maki.NIL`) and other non-table values are shared, not copied.

A table reachable twice is copied once, so shared references and cycles
survive. Pass {noref} to copy such a table every time it is reached. That
is faster for trees, but a cycle then fails with an error.

**Parameters:**

- `{orig}` (`any`) Value to copy.
- `{noref?}` (`boolean?`) Optional. When true, shared references are not tracked.

**Returns:** (`any`) The copy.

**Example:**

```lua
local input = maki.json.decode('{"files": []}')
local copy = maki.deepcopy(input)
copy.mode = "strict"
print(maki.json.encode(copy)) -- {"files":[],"mode":"strict"}
```

---

### `maki.NIL` {#maki-NIL}

```lua
maki.NIL
```

`maki.NIL` is a value that stands for JSON null, like `vim.NIL`. `maki.json.decode` returns it for every null unless `luanil` says otherwise. Every conversion to JSON turns it back into null. So a decode, change, encode pass loses no key. It is truthy and not equal to nil, so test `x == maki.NIL` for null. Tool handler arguments and event payloads use nil for null instead, never `maki.NIL`.

**Example:**

```lua
local t = maki.json.decode('{"a": null}')
assert(t.a == maki.NIL)
print(maki.json.encode(t)) -- {"a":null}
```


## maki.pack {#maki-pack}

Declare global packages and inspect package state.

`add` is available only in the global `init.lua`. `get` is read-only
and is available in project config and packages.

---

### `maki.pack.add()` {#maki-pack-add}

```lua
maki.pack.add({specs}, {opts?})
```

Declare global packages after the global `init.lua` finishes.

**Parameters:**

- `{specs}` (`table`) Sources or tables with `src`, `name`, `version`, and `data`.
- `{opts?}` (`table?`) `confirm` controls source confirmation. `load` is a

  boolean or a custom loader function.


**Example:**

```lua
maki.pack.add({
  { src = "https://github.com/user/maki-goal", version = "main" },
})
```

---

### `maki.pack.get()` {#maki-pack-get}

```lua
maki.pack.get({names?}, {opts?})
```

Get package state without changing the installed set.

**Parameters:**

- `{names?}` (`table?`) Package names. Omit for all managed packages.
- `{opts?}` (`table?`) Reserved. Omit it.

**Returns:** (`table?`, `string?`) Package records with `spec`, `path`, `rev`, and
  `active`, or nil plus an error when the pack lockfile cannot be read.


## maki.api {#maki-api}

Plugin registration. This is where you tell maki about your tools,
slash commands, and prompt contributions.

Most plugins only need `register_tool` and maybe `register_prompt_hint`.
Call these at the top level of your plugin file (during load).

```lua
maki.api.register_tool({ name = "greet", ... })
maki.api.register_prompt_hint({ slot = "tool_usage", content = "..." })
```

---

### `maki.api.register_tool()` {#maki-api-register_tool}

```lua
maki.api.register_tool({spec})
```

Register a new tool the agent can call. This is the main way plugins add
capabilities to the agent. The tool is queued during plugin load and
committed to the registry once the plugin finishes loading.

Your {spec} table must include a name, a description (the model reads it
to decide when to use the tool), a JSON Schema for the input, and a handler
function. The handler receives `(input, ctx)` and returns either a plain
string or a table with richer output fields.

`ctx:output_mode()` returns "compact" when the result goes to the model and
"full" when code such as `code_execution` made the call. Apply output limits
only in compact mode.

**Parameters:**

- `{spec}` (`table`) Tool specification:
  - `name` (`string`) Required. ASCII identifier, up to 64 chars ([a-zA-Z_][a-zA-Z0-9_]*).
  - `description` (`string`) Required. Non-empty description shown to the model.
  - `schema` (`table`) Required. JSON Schema object describing the tool's input parameters.
  - `handler` (`function`) Required. Called with `(input, ctx)` when the tool is invoked.
    Must return a string or a table with any of these fields:
    - `llm_output` (`string`) Result text for the model or the calling code.
    - `is_error` (`boolean`) When true, the result is treated as an error.
    - `body` (`BufHandle`) Rich rendered body shown in the UI.
    - `header` (`BufHandle`) One-line header shown before the body.
    - `format` (`string`) "plain" (default) or "markdown".
    - `annotation` (`string`) Short label shown next to the tool call.
    - `written_path` (`string`) Path of a file written by the tool.
    - `diff_path` (`string`) Path for a diff output block.
    - `diff_before` (`string`) Before text of the diff.
    - `diff_after` (`string`) After text of the diff.
    - `image` (`maki.image.Image|table`) An Image, or { media_type: string, data: string } base64 image.
    - `state` (`any`) Serializable state forwarded to restore.
  - `audiences` (`string[]`) Which model audiences see the tool. Values: "main", "sub", "all". Default: all audiences.
  - `kind` (`string`) Optional grouping label (e.g. "filesystem").
  - `timeout` (`number`) Execution timeout in seconds. 0 or false disables. Default: inherits agent deadline.
  - `header` (`function`) Optional. Called before execution, returns a string or BufHandle for the one-line header.
  - `restore` (`function`) Optional. Called to re-render a previous tool result. Receives `(tool_name, input, output, ctx)`.
  - `start` (`function`) Optional. Called when the tool call starts, before the handler runs.
  - `describe` (`function`) Optional. Returns a custom description string for the current context.
  - `examples` (`table`) Optional. Array of example input objects for documentation.
  - `permission_scopes` (`string|function`) Field name in schema (string) or `function(input)` returning a list of path scopes that need write permission. Declaring it is what puts the tool in front of the permission prompt, and it requires `permission`.
  - `permission` (`string`) The capability the tool reaches: "fs_read", "fs_write", "net", "run", or "env". Your plugin must hold it, and so must any plugin that pre-approves the tool, hooks it, or hands it to a session. Without `permission_scopes` the tool never prompts, the right declaration for read-only tools. A tool declaring neither costs those plugins every permission.
  - `mutable_path` (`string`) Schema field name (type: string) for the primary path the tool writes. Required with `permission = "fs_write"`. Declaring it is what gets the tool, from the dispatcher and never from the handler: serialization of concurrent calls on that file, the stale-read rejection, the plan-mode block, and the permission boundary check.
  - `start_annotation` (`string|table`) Schema field used to annotate the start header with a count (string) or timeout (`{ field, kind="timeout" }`).

**Example:**

```lua
maki.api.register_tool({
  name = "word_count",
  description = "Count words in a file.",
  kind = "read",
  schema = {
    type = "object",
    properties = { path = { type = "string", description = "File path" } },
    required = { "path" },
  },
  handler = function(input)
    local f = io.open(input.path, "r")
    if not f then return { llm_output = "file not found", is_error = true } end
    local n = 0
    for _ in f:read("*a"):gmatch("%S+") do n = n + 1 end
    f:close()
    return tostring(n) .. " words"
  end,
})
```

---

### `maki.api.register_permission_rule()` {#maki-api-register_permission_rule}

```lua
maki.api.register_permission_rule({spec})
```

Declare an agent permission rule for a native tool. Use it to pre-allow
(or pre-deny) tool calls on paths your plugin owns, like a storage
directory outside the working dir, so the user is not prompted for them.

Rules live as long as the plugin is loaded: a reload replaces them, and a
reload that registers none clears the old ones. User config and session
deny rules always win over a plugin allow.

An allow is delegation, not escalation: it needs the `permission` the
target tool declares, so a plugin can only pre-approve what it could
already do itself. A deny needs no permission. An allow on a tool that
never prompts (`permission` without `permission_scopes`) has nothing to
pre-approve and is dropped.

Allows are checked once the plugin finishes loading, so a plugin may
pre-approve a tool it registers itself. One that does not hold up (no such
tool, a tool with no `permission_scopes` that is never checked, or a
permission the plugin lacks) is dropped with a warning in the log while the
rest of the plugin loads. Without the rule the call simply prompts.

**Parameters:**

- `{spec}` (`table`) Rule specification:
  - `tool` (`string`) Required. Native tool name (e.g. "edit", "write").
    MCP tools and the "*" wildcard are not allowed.
  - `scope` (`string`) Required. Scope pattern the rule applies to, e.g.
    "/abs/dir/**" for a directory subtree. An allow whose
    pattern matches every scope ("*", "**", "/*", "/**") is
    refused: name the paths or commands it covers. A deny may
    cover everything.
  - `effect` (`string`) Optional. "allow" (default) or "deny".

**Example:**

```lua
maki.api.register_permission_rule({
  tool = "write",
  scope = notes_dir .. "/**",
})
```

---

### `maki.api.register_command()` {#maki-api-register_command}

```lua
maki.api.register_command({spec})
```

Register a slash-command that appears in the user input bar.

Slash commands let the user trigger plugin actions by typing `/name` in the
input. Use them for interactive workflows that do not need the model, like
browsing memory files or toggling settings.

**Parameters:**

- `{spec}` (`table`) Command specification:
  - `name` (`string`) Required. The command name (e.g. "/hello"; a leading
    slash is added when missing).
  - `description` (`string`) Optional. Short description shown in the command palette.
  - `nargs` (`integer|string`) Optional. How many arguments the command
    takes, spelled like nvim's nargs: 0 (default),
    1, "?" (zero or one), "*" (any number), or "+"
    (one or more). An argument is a whitespace
    separated word. Type more than allowed and the
    command quietly stops matching: the input goes
    to the model as a normal message. Only the upper
    bound is checked, so with "+" you still need to
    handle an empty `opts.args` yourself.
  - `handler` (`function`) Required. Called when the user runs the command,
    with one opts table: `opts.args` is the raw
    argument string (whitespace kept, may be empty)
    and `opts.fargs` is the same split into words.

**Example:**

```lua
maki.api.register_command({
  name = "/hello",
  description = "Say hello",
  handler = function()
    maki.ui.flash("Hello from my plugin!")
  end,
})
```

---

### `maki.api.register_prompt_hint()` {#maki-api-register_prompt_hint}

```lua
maki.api.register_prompt_hint({spec})
```

Add a piece of text to an aggregate prompt slot. Multiple plugins can each
contribute to the same slot, and all contributions are concatenated.

Good for things like tool usage guidelines or extra context that should
appear alongside other plugins' hints. If you need to own the whole slot
(e.g. identity or tone), use `set_prompt` instead.

Throws if you pass a singleton slot name.

A function `content` is called before every run. Its first value goes in
the system prompt, and later changes reach the model as a context update
until the next compaction.

**Parameters:**

- `{spec}` (`table`) Hint specification:
  - `slot` (`string`) Required. Aggregate slot name (e.g. "tool_usage", "conventions").
  - `content` (`string|function`) Required. Static text, or a `function()` that returns a string. Max 1 MiB.
  - `prompt` (`string|string[]`) Optional. Restrict to specific prompt ids (e.g. "system").

**Example:**

```lua
maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Prefer **grep** over reading entire files.",
})
```

---

### `maki.api.register_options()` {#maki-api-register_options}

```lua
maki.api.register_options({spec})
```

Declare the options your plugin accepts under `plugins.<name>` in
`maki.setup`, and get back what the user set merged with your defaults.
Call it once, at the top level of your plugin file.

An unknown key, a wrong type, or a value below `min` fails the plugin
load with a clear message, so users catch typos right away. Bad specs
fail the load too. The specs also feed the generated configuration docs.

**Parameters:**

- `{spec}` (`table`) Map of option name to a spec table:
  - `default` (`boolean|number|string`) Optional. Used when the user sets nothing. Its Lua type becomes the option type.
  - `type` (`string`) Required when there is no default: "boolean", "integer", "number", or "string".
  - `min` (`number`) Optional. Minimum accepted value, numeric options only.
  - `desc` (`string`) Required. One line shown in the configuration docs.

**Returns:** (`table`) Merged options: the user's value where set, otherwise the default, or nil when neither exists.

**Example:**

```lua
local opts = maki.api.register_options({
  timeout_secs = { default = 120, min = 5, desc = "Kill the command after this many seconds." },
  max_output_lines = { type = "integer", desc = "Override agent.max_output_lines for this tool." },
})
```

---

### `maki.api.set_prompt()` {#maki-api-set_prompt}

```lua
maki.api.set_prompt({spec})
```

Replace a complete prompt or one named section. Section replacements
preserve additive hints registered through `register_prompt_hint`.

Omit `slot` to replace a whole template; this requires one explicit prompt
ID. A replacement inserts section defaults, hints, and instructions only
through explicit `{{section}}` and `{{instructions}}` markers. An empty
string clears a template or section. The alphabetically last plugin wins,
and its last registration wins within that plugin. Unload removes its values.

Prompt IDs: `system`, `research`, `general`, and `plan`.
Sections: `identity`, `tone`, `tool_usage`, `efficient_tools`, `conventions`,
`after_instructions`, `objectivity`, and `completion`.

Identity and tone are available in system, research, and general prompts.
Sections target the main agent when `prompt` is omitted. Only identity and
tone accept a function called before every run. Its first value goes in the
system prompt. Later changes reach the main agent as a context update.
Other replacements require static strings.
A known section missing from the resolved template is ignored, with a warning
for explicit prompt targets.

**Parameters:**

- `{spec}` (`table`) Prompt replacement:
  - `slot` (`string`) Optional. Named section, e.g. "identity", "tone", "conventions".
  - `content` (`string|function`) Required. Replacement text, including empty text. Max 1 MiB. Only identity and tone accept functions.
  - `prompt` (`string|string[]`) Optional for sections; defaults to "system". A whole replacement requires one explicit prompt ID.

**Example:**

```lua
maki.api.set_prompt({
  prompt = "system",
  slot = "conventions",
  content = "State uncertainty and cite your evidence.",
})
maki.api.set_prompt({
  prompt = "system",
  content = "You help analyse information.\n{{tool_usage}}",
})
```

---

### `maki.api.get_tools()` {#maki-api-get_tools}

```lua
maki.api.get_tools({opts?})
```

Return a list of all registered tools. Useful for building UI that shows
available tools or for checking which tools are enabled.

Each entry has the tool's name, schema, audiences, and an `enabled` flag.
Describe callbacks are not invoked (the static description is used).

**Parameters:**

- `{opts?}` (`table?`) Options:
  - `config` (`table`) Optional config table with a `disabled_tools` string[] field used to compute the `enabled` flag on each entry.

**Returns:** (`table[]`) Array of tool entries: { name, schema, audiences, kind?, enabled }.

**Example:**

```lua
local tools = maki.api.get_tools()
for _, t in ipairs(tools) do
  print(t.name, t.enabled)
end
```

---

### `maki.api.get_tool()` {#maki-api-get_tool}

```lua
maki.api.get_tool({name})
```

Look up a single tool by name. Returns its metadata table or nil if the
tool does not exist. For Lua-registered tools the returned table also
includes `header` and `restore` handle functions (wrapped so they never
throw).

`tool_search` and MCP tools (`server__tool` names) never enter the
registry. For them the table holds only `name`, `header`, and the
`format` ("plain" or "markdown") of the standalone call, enough for a
composing tool like batch to draw a child the same way. Any
`server__tool` shaped name resolves whether or not a server offers it,
so this is not an existence check for MCP tools.

**Parameters:**

- `{name}` (`string`) Exact tool name.

**Returns:** (`table|nil`) Tool entry with fields { name, schema, audiences, kind?, header?, restore?, format? }, or nil if not found.

**Example:**

```lua
local t = maki.api.get_tool("bash")
if t then
  print("bash audiences:", table.concat(t.audiences, ", "))
end
```

---

### `maki.api.run_command()` {#maki-api-run_command}

```lua
maki.api.run_command({cmdline})
```

Runs a slash command by name, exactly as typing it in the input would.
Works for built-ins, custom `/project:` and `/user:` commands, MCP
prompts, and commands other plugins registered.

Use it to alias a command you like under a name you prefer, instead of
reimplementing what it does. See `maki.ui.action` for the same idea
applied to keybound UI actions.

Pass the whole command line, arguments included: `"/cd ~/src"`. The
leading slash is optional. Names match exactly apart from case, so a typo
reports an error instead of running the closest command, and a cycle of
aliases stops with one too.

This returns as soon as the command has been dispatched, not when it
finishes, so aliasing something long-running like `/compact` does not
block your handler.

A command outside the calling plugin's grants returns an error, and a few
only run when you type them. [Commands](/docs/commands/#plugin-access)
lists what each one needs.

**Parameters:**

- `{cmdline}` (`string`) Command line, e.g. `"/new"` or `"/cd ~/src"`.

**Returns:** (`boolean|nil`, `string|nil`) `true` once dispatched, or nil and an error message for an unknown or denied command.

**Example:**

```lua
-- /resume as an alias for the built-in session picker:
maki.api.register_command({
  name = "/resume",
  description = "Alias for /sessions",
  handler = function()
    local ok, err = maki.api.run_command("/sessions")
    if not ok then
      maki.ui.flash("could not run /sessions: " .. err)
    end
  end,
})
```

---

### `maki.api.create_autocmd()` {#maki-api-create_autocmd}

```lua
maki.api.create_autocmd({event}, {opts})
```

Listen for one or more events. Returns an id you can pass to
`del_autocmd` later to remove the listener.

Built-in events fired by the host: `"TurnStart"`, `"TurnEnd"`,
`"TurnError"`, `"ToolStart"`, `"ToolDone"`, `"AutoCompacting"`,
`"CompactionDone"`, `"PlanReady"`, `"SessionReset"`, `"SessionEnd"`,
`"SessionFocusChanged"`, `"SessionStatusChanged"`, `"SessionTitleChanged"`,
`"SessionCwdChanged"`, `"TaskStatusChanged"`, `"TaskStopped"`, `"TaskFocusChanged"`, `"ModelChanged"`, `"InputChanged"`,
`"ModeChanged"` and `"FileIndexReady"`. Plugins can also fire their own
events with `exec_autocmds`.

Every host event carries `data.session_id` except `"FileIndexReady"`,
which is about a directory rather than a session, and `"ModeChanged"`,
since the mode is the same in every session. For `"SessionReset"` and
`"SessionEnd"` that is the session being left behind, the other events
name the session now running or focused. What each event adds:

- `"ToolStart"`, `"ToolDone"`: `data.tool_id` and `data.tool`.
- `"ToolDone"` adds `data.is_error` and `data.bytes`, the size of the
  text the model reads. A call that ran also carries `data.duration_ms`
  and `data.input`, the input after every `tool.*.input` layer. A call
  that never ran, like a cancelled one, has neither.
- `"TurnStart"`: `data.text`, the message that started the turn.
- `"TurnEnd"`: `data.reason` (`"finished"`, `"max_tokens"`,
  `"max_turns"`, `"cancelled"`, or `"dropped"` when an
  `agent.user_message` layer refused the message), `data.usage` (four
  token fields, cache included), `data.cost`, `data.list_cost`, `data.context_size`,
  `data.context_window`, and `data.num_turns` (model round-trips the
  turn took). `list_cost` is the un-subsidised list price and `cost` is
  the real bill, so a budget plugin charges against whichever one it
  wants.
- `"AutoCompacting"`: `data.context_size` and `data.context_window` at
  trigger time.
- `"CompactionDone"`: `data.context_size_before`,
  `data.context_size_after`, `data.context_window`, and `data.summary`,
  the text that replaced the history.
- `"PlanReady"`: `data.path`, the absolute path of the plan file the
  agent just wrote. Fires once per draft. Plan state is per session, so
  pass `data.session_id` to `maki.plan.read`.
- `"SessionFocusChanged"`: `data.previous_session_id`, absent on the
  first focus at startup.
- `"SessionStatusChanged"`: `data.status` (`"working"`, `"needs_input"`,
  or `"idle"`), `data.title`, and `data.focused` (boolean).
- `"SessionTitleChanged"`: `data.title` and `data.focused` (boolean),
  when the title changes (rename or auto-generation).
- `"SessionCwdChanged"`: `data.cwd`, the session's new working
  directory, when `/cd` changes it.
- `"TaskStatusChanged"`: `data.id`, `data.name`, and `data.status`
  (`"working"`, `"idle"`, `"done"`, or `"error"`), when a subagent starts
  or changes status. A task a plugin opened with `maki.agent.session`
  carries `data.plugin`, and `"idle"` is such a task between prompts. A
  task that comes back from disk already finished stays quiet, so
  reloading a session does not replay old tasks.
- `"TaskStopped"`: `data.id`, when the user stops a task with Esc in its
  tab or through `maki.task.stop`. A plugin task carries `data.plugin`,
  and by then it is already gone from `maki.agent.list()`.
  `"TaskStatusChanged"` compares tasks once per frame, so it can miss a
  task that opened and stopped in the same frame. This event never does,
  so an owner can rely on it to drop work it still has queued.
- `"TaskFocusChanged"`: `data.id`, the task now on screen (`"main"` or a
  subagent's id, what `ctx:task_id()` reports inside a tool). Fires for
  the chat cycling keys, `maki.task.focus`, and a session switch that
  lands on another task.
- `"ModelChanged"`: `data.model` in the shape `maki.model.get` returns,
  plus `data.previous_spec`. Picking the model already in use stays
  quiet, and so does startup.
- `"InputChanged"`: `data.text`, `data.cursor`, `data.anchor`,
  `data.version` and `data.session_id`, as
  [`maki.ui.input`](#maki-ui-input) reports them. `data.cursor_only` is
  true when only the cursor or the selection moved. `data.source` names
  the plugin whose `maki.ui.input_edit` made the only change since the
  last `"InputChanged"`, and is nil otherwise, so a handler can skip its
  own edits. A package goes by its
  [package name](/docs/packages/#install-from-git), and `init.lua` code
  by `"global/init.lua"` or `"project/init.lua"`. Fires at most once per
  screen redraw, and again for the input of a tab that takes focus.
  [Marks](#input-marks) alone do not fire it, so a handler can call
  `maki.ui.input_decorate` without looping.
- `"ModeChanged"`: `data.old_mode` and `data.new_mode`, each `"n"` or
  `"i"`. Fires whenever `maki.fn.mode()` changes: on `maki.ui.set_mode`,
  and when the chat input gains or loses the keyboard, as when a picker
  opens. Use it to draw a mode indicator. Like `"InputChanged"`, it fires
  only with a UI.
- `"FileIndexReady"`: `data.root`, the absolute directory that was
  walked, `data.files`, how many paths the walk left, and `data.crashed`
  and `data.truncated`, the two ways that list is not the whole tree.
  Fires once per walk that ends, whatever `maki.fs.fuzzy_files` or the
  `Ctrl+S` picker started it, so a plugin ranking files asks again
  instead of polling. A walk cancelled before it ended stays quiet.
  `data.root` is absent for a directory with no UTF-8 spelling, and so is
  the `root` of the call that asked for the walk, so a plugin matching
  the two takes an event without a root as a reason to ask again. Asking
  again only ever re-reads the root the plugin passed.

Conversation content needs the `session_read` permission. Without it a
host event still arrives, carrying only its metadata fields:
`"TurnStart"` without `data.text`, `"ToolDone"` without `data.input`,
`"CompactionDone"` without `data.summary`, and `"InputChanged"` without
`data.text`. Maki warns once per plugin and event, naming what was left
out. Events a plugin fires under its own names arrive as fired.

`"TurnEnd"` fires once per turn and only for the main session, so
subagent turns never show up. A manual `/compact` ends its run without
ending a turn, so it stays quiet too.

Drivers are not all caught up. `"TurnStart"` and `"PlanReady"` come from
`maki-ui` only. `maki-acp` runs the agent on its own loop and does not
call the dispatcher yet, so plugins loaded under ACP receive no turn
events. Everything else fires under `maki -p` and sdk mode as well.

`"SessionEnd"` is the teardown signal: it fires first so handlers can
still inspect or stop the session's jobs, then session-owned jobs are
reaped. `data.reason` names the path it came from: `"reset"` (TUI
`/new`), `"load"`, `"delete"` (tab closed), `"shutdown"`, `"reload"`
(`/reload` is rebuilding the plugin host, and the session carries on in
the new one), `"replaced"` (an ACP client took the session's place), or
`"completed"` (a headless run finished).

On `"shutdown"`, `"reload"`, `"replaced"`, and `"completed"` the host is
already tearing down, so the UI is detached (`maki.fn` roundtrips fail
right away) and every handler shares one grace period. `data.deadline_ms`
is how much of it is left at dispatch, so write state out with `maki.fs`
and do not park. On the other reasons nothing waits and `data.deadline_ms`
is nil.

`"SessionReset"` stays TUI-only (`/new`) and fires on the same path as
`"SessionEnd"` with `reason = "reset"`.

Jobs started inside a callback die with the dispatch unless you await
them there (`jobwait`) or hand them to a session
(`scope = { session = ... }`).

**Parameters:**

- `{event}` (`string|string[]`) Event name or list of names.
- `{opts}` (`table`) Options:
  - `callback` (`function`) called with an ev table `{ id, event, match, data }`.
  - `once` (`boolean`) remove the handler after it fires once (default false).
  - `pattern` (`string|string[]`) only fire when the pattern matches. `"*"` matches everything. Omit to match all.

**Returns:** (`integer`) Autocmd id.

**Example:**

```lua
local id = maki.api.create_autocmd("TurnEnd", {
  callback = function(ev)
    print("turn ended: " .. ev.event)
  end,
})
```

---

### `maki.api.del_autocmd()` {#maki-api-del_autocmd}

```lua
maki.api.del_autocmd({id})
```

Remove a previously registered autocmd. Does nothing if the {id}
does not exist.

**Parameters:**

- `{id}` (`integer`) Id returned by `create_autocmd`.

**Example:**

```lua
maki.api.del_autocmd(id)
```

---

### `maki.api.exec_autocmds()` {#maki-api-exec_autocmds}

```lua
maki.api.exec_autocmds({event}, {opts?})
```

Fire one or more events manually. Every matching autocmd callback
runs to completion before this function returns.

A handler may suspend, so this call may too.

**Parameters:**

- `{event}` (`string|string[]`) Event name or list of names to fire.
- `{opts?}` (`table?`) Options:
  - `pattern` (`string`) passed to callbacks as `ev.match`.
  - `data` (`any`) arbitrary value passed as `ev.data`.

**Example:**

```lua
maki.api.exec_autocmds("MyEvent", {
  pattern = "init",
  data = { msg = "hello" },
})
```

---

### `maki.api.register_action()` {#maki-api-register_action}

```lua
maki.api.register_action({name}, {callback}, {opts?})
```

Registers {callback} as an action that `maki.ui.action` runs. This is
how a user's `init.lua` reaches your plugin, since `require` only sees
the calling plugin's own files.

The action is named after your plugin: `register_action("accept", ...)`
in a plugin installed as `completion` registers `"completion.accept"`. In
the global or project `init.lua` the prefix is `user`, like
`"user.paste_plain"`. A plugin named `user` cannot register actions, so no
plugin can replace the ones a user's keys call.

The callback runs as your plugin, inside the caller's key handler, and
`maki.ui.action` returns its value. Return `true` when it handled the
request, and `false` or nothing so the caller can try something else.
Registering a name again replaces the callback, and the action goes away
when your plugin unloads. When both `init.lua` files register a name, the
project one wins until it is gone.

`opts.capability` is the price of the action: the permissions a caller
must hold. Without it, the price is every permission your plugin holds.
With `{}`, any caller gets in, and the callback holds no permission. A
caller that falls short gets `nil` and an error naming the missing
permission.

The price also caps the callback, whoever calls it, so list everything
it uses. A call that needs a permission outside the price fails as it
would in a plugin without that permission. The cap covers the tasks,
timers and jobs the callback starts, and the tools, permission rules,
slot layers and actions it registers. Keymaps and autocmds it sets run
later with all your grants. Code of other plugins that runs meanwhile,
like their autocmds, keeps its own grants. For example,
`completion.accept` writes file paths into the input that Enter then
sends, so it lists `fs_read`, `input`, `session_read`, `agent` and
`session_write`.

Also bind a default key for users who do not
[compose keys](/docs/keybindings/#composing-keys), preferably an Alt chord,
since your binding [shadows theirs](/docs/keybindings/#overriding-keybindings)
on the same key.

Throws when `capability` lists a permission your plugin does not hold, or
when {name} is empty or has a `.`.

**Parameters:**

- `{name}` (`string`) The name after the prefix, like `"accept"`.
- `{callback}` (`function`) Called with no arguments. Return `true` when handled.
- `{opts?}` (`table|nil`) `{ capability = { "fs_read", ... } }`: what a caller must hold. Default: every permission your plugin holds.

**Returns:** (`string`) The full name to pass to `maki.ui.action`, like `"completion.accept"`.

**Example:**

```lua
-- Writes a file path into the input, so a caller must hold what that takes:
maki.api.register_action("accept", function()
  if not Menu.open() then return false end
  Menu.accept()
  return true
end, { capability = { "fs_read", "input", "session_read", "agent", "session_write" } })
```

---

### `maki.api.declare_slot()` {#maki-api-declare_slot}

```lua
maki.api.declare_slot({name}, {default}, {opts?})
```

Create a named extension point owned by your plugin. You provide a
{default} function, and other plugins can wrap it with layers using
`set_slot`. The returned callable runs the full chain: outermost layer
first, then inward, ending at {default}.

{opts} prices what a layer from another plugin pays to steer your chain.
You set it, because you are the only one who knows what your default does
with the arguments it is handed. Pass `{ capability = { "net" } }` to
charge the permissions you name, all of them at once; `{ capability = {} }`
to let anyone layer for free, which is the honest price for a slot whose
arguments are inert; or leave {opts} out to charge every permission. You
can only name permissions your own plugin holds.

Throws if another plugin already owns a slot with the same {name}, or
if {name} starts with `"tool."`, `"ui."`, `"agent."`, or `"permission."`,
which the host fires itself.
The name stays yours across an unload: nobody else can take it over, or
re-declare it cheaper, while maki runs.

The chain is async: the default and every layer may park (`maki.fs.*`,
`maki.fn.jobwait`, `maki.agent.call_tool`, ...), and so does the
returned callable. Call it from a tool handler, a command, or an
autocmd, rather than from a `header` or `restore` function, which
cannot wait. The chain runs in your task, so cancelling the caller
cancels the layers it is waiting on.

**Parameters:**

- `{name}` (`string`) Unique slot name, e.g. `"myplugin.render"`.
- `{default}` (`function`) Default implementation, called when no layers wrap it.
- `{opts?}` (`table|nil`) `{ capability = { "net", ... } }`: what a layer from another plugin pays.

**Returns:** (`function`) Callable that dispatches through all layers.

**Example:**

```lua
-- anyone may layer this one: it only uppercases the text it is given
local render = maki.api.declare_slot("myplugin.render", function(text)
  return text:upper()
end, { capability = {} })
print(render("hello")) -- HELLO
```

---

### `maki.api.set_slot()` {#maki-api-set_slot}

```lua
maki.api.set_slot({name}, {wrapper})
```

Add a layer around an existing (or future) slot. Layers wrap the
default from the outside in. Each layer receives `prev` as its
first argument. Call `prev(...)` to continue down the chain.
Calling `prev` more than once throws.

You can call this before the owner runs `declare_slot`. The layer
is queued and attached when the slot is declared.

A layer may park, and one that throws is skipped: the chain continues
as if it had returned `prev(...)` untouched, so a broken layer never
takes the seam down with it.

Layers wrap in registration order, so the last one registered runs
first and sees the value before the others do.

Maki fires two slots around the plan form, both with
`ev = { path, session }`. `ui.plan_form.actions` asks for the form's
menu, and `ui.plan_form` asks whether the form opens at all. Both are
documented under [maki.plan](/docs/lua-api/#maki-plan).

Maki fires two slots per tool itself: `tool.<name>.input` before
permissions look at the call, and `tool.<name>.output` on the text it
produced. Both take `function(prev, value, ctx)` and answer with a
table to replace the value, nothing to leave it alone, or
`nil, reason` to stop the call. An input layer can also answer
`value, { ask = reason }` to make the user approve the call. Name the
tool `*` (`tool.*.input`) to wrap every tool. What a layer costs is in
the [Hooks price table](/docs/hooks/#rules).

The agent loop fires `agent.user_message`, `agent.stop`,
`agent.compact.before`, and `agent.compact.prepare`, with the same
contract. Wrapping one costs every permission. See
[Hooks](/docs/hooks/).

`permission.prompt` fires where the permission prompt would show, with
the prompt as its default. A layer takes `function(prev, req, ctx)` and
answers `{ decision = "allow" }` or `{ decision = "deny", guidance = … }`,
or passes the call on with `prev(req, ctx)`. A verdict decides that one call
and records no rule, and an escalated call (`ask`) is never offered to a
layer. It costs what a layer on the same call does. See
[Hooks](/docs/hooks/#permission-prompt).

Wrapping a slot another plugin declared steers a chain that plugin's
callers trust, so it costs whatever the owner priced it at in
`declare_slot`: the capabilities it named, every permission if it named
none, or nothing at all if it declared its arguments inert. Layering a slot
you declared yourself is free. Like the `tool.*` slots, this is decided
when the chain fires: the call skips a layer that is not entitled and
carries on, and a reload that changes what you hold takes effect on the
next call. A layer set inside an
[action](#maki-api-register_action) callback is weighed against the
action's price too.

**Parameters:**

- `{name}` (`string`) Slot name to wrap.
- `{wrapper}` (`function`) Layer: `function(prev, ...)`. Call `prev(...)` to continue.

**Example:**

```lua
maki.api.set_slot("myplugin.render", function(prev, text)
  return prev("[" .. text .. "]")
end)
```

---

### `maki.api.get_slots()` {#maki-api-get_slots}

```lua
maki.api.get_slots()
```

List all known slots and their current state. Useful for debugging
which plugins own or wrap each slot.

`capability` is the list of permissions a layer from another plugin pays,
and is absent on a slot whose owner named no price, which costs every
permission.

**Returns:** (`table`) Map of slot name to `{ owner, declared, fillers, capability }`.

**Example:**

```lua
for name, info in pairs(maki.api.get_slots()) do
  print(name, info.owner, info.declared)
end
```


## maki.plan {#maki-plan}

Plan-mode surface for plugins.

Read the plan, and shape the plan form by layering the two slots maki
fires around it. Plan state is per session, so every call takes an
optional `session` and defaults to the focused tab.

`ui.plan_form.actions` is the menu. The default answers with the
built-in rows, each `{ id, label, desc, action }`, and a layer
appends, reorders or drops them before returning the list. Every row
needs an `id` no other row uses, since that is how a later layer finds
it. Anything past the 32nd row is dropped.

A row carrying a `handler` has that function called on the Lua thread
with `{ session, path, parallel }` when the user picks it. The row's
`action` runs after the handler returns, unless the handler returned
`false` or failed, so copying a built-in row and adding a handler
keeps the built-in outcome. Drop the `action` to replace it.

`ui.plan_form` is the form itself. A layer that answers `false` keeps
it closed and renders the plan however it likes.

Layering either slot costs every permission, the price of steering a
call whose reach nobody declared: a row decides what pressing Enter
does, up to a build-mode turn with every tool behind it.

Unloading your plugin hands the form back and reaps its row handlers.

```lua
-- A row of your own, next to the built-in ones:
maki.api.set_slot("ui.plan_form.actions", function(prev, ev)
  local rows = prev(ev)
  table.insert(rows, {
    id = "commit_and_implement",
    label = "Commit and implement",
    desc = "Commit the plan file first, then implement it",
    handler = function(opts)
      maki.fn.system({ "git", "commit", "-am", "plan" })
      maki.session.set_mode("build", { session = opts.session })
      maki.session.prompt("Implement " .. opts.path, { session = opts.session })
    end,
  })
  return rows
end)

-- Render the plan yourself for as long as this plugin is loaded:
maki.api.set_slot("ui.plan_form", function(prev, ev)
  local plan = maki.plan.read({ session = ev.session })
  return false
end)
```

---

### `maki.plan.read()` {#maki-plan-read}

```lua
maki.plan.read({opts?})
```

Read the current plan state. Returns `{ mode, path, content, ready }`:
- `mode` is `"plan"` or `"build"`.
- `path` is the absolute plan path once the session has one, else `nil`.
- `ready` is `true` once the agent has written the plan file.
- `content` is the file contents, `nil` when the plan is not ready or the
  read failed.

Requires the `session_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{opts?}` (`table?`) `session` (string?) Session id, defaults to focused.

**Returns:** (`table|nil`, `string|nil`) Plan snapshot table, or nil and an error.

**Example:**

```lua
local plan, err = maki.plan.read({ session = id })
if plan and plan.ready then
  print(plan.path, plan.content)
end
```


## maki.agent {#maki-agent}

Subagent primitives for plugins that need to talk to an LLM.

This module gives you the building blocks: resolve which model to use,
build a system prompt, list available tools, call a tool directly, or
open a full session with its own conversation history.

Policy like retries, validation, and concurrency lives in the calling
plugin, not here.

```lua
local tools = maki.agent.tools(ctx, { audience = "general_sub" })
local sess = maki.agent.session(ctx, {
  system = "You are a helpful assistant.",
  tools = tools,
})
local r = sess:prompt("Hello!")
print(r.text)
sess:close()
```

A CI monitor for each chat session the user opens. It wakes the agent
once when CI turns red and stays quiet until CI is green again. Each
wake costs a full agent turn, and the model words the same failure
differently every round, so comparing replies would wake it every time.
`SessionFocusChanged` also fires after `/reload`, so the monitor comes
back on its own.

```lua
local tasks, failing = {}, {}

local function start(sid)
  if tasks[sid] then return end
  tasks[sid] = maki.async.spawn(function()
    while true do
      maki.async.sleep(5 * 60 * 1000)
      -- Prompts are denied here, so this needs an allow rule like `gh run list *`.
      local sess = maki.agent.session({
        session = sid,
        name = "ci-monitor",
        system = "Run `gh run list -L 3`. Reply exactly OK if all runs passed, else a 3-line failure summary.",
        tools = (maki.agent.tools({ session = sid, audience = "general_sub", only = { "bash" } })),
      })
      -- nil once the chat session is gone. SessionEnd below stops the loop.
      if sess then
        local r, err = sess:prompt("Check CI now.")
        sess:close()
        local text = not err and r.text:match("^%s*(.-)%s*$")
        if text == "OK" or text == "OK." then
          failing[sid] = nil
        elseif text and text ~= "" and not failing[sid] then
          failing[sid] = true
          maki.session.notify("[ci-monitor] " .. text, { session = sid, wake = true })
        end
      end
    end
  end)
end

maki.api.create_autocmd("SessionFocusChanged", {
  callback = function(ev) start(ev.data.session_id) end,
})
maki.api.create_autocmd("SessionEnd", {
  callback = function(ev)
    local sid = ev.data.session_id
    if tasks[sid] then tasks[sid]:cancel() end
    tasks[sid], failing[sid] = nil, nil
  end,
})
```

---

### `maki.agent.resolve_model()` {#maki-agent-resolve_model}

```lua
maki.agent.resolve_model({ctx}, {opts?})
```

Look up the model that the current agent is using, or pick a cheaper one.
You might want a cheaper model for simple subtasks (summaries, classification)
without hard-coding a model name.

The returned table has fields: `id` (string), `tier` (string),
`provider` (string), `spec` (string).

**Parameters:**

- `{ctx}` (`LuaCtx`) Agent context.
- `{opts?}` (`table?`) Optional fields:
  - `tier` (`string?`) target tier, e.g. `"fast"`, `"mid"`, `"best"`. Clamped to
    the parent tier so you cannot escalate. Resolved by
    `maki.model.tier`, so a `/model` pick for the tier wins over the
    session provider's curated default.
  - `spec` (`string?`) exact model spec string, e.g. `"claude-3-5-haiku-20241022"`.
    Takes precedence over `tier`.

**Returns:** (`table?`, `string?`) Model table on success, or `(nil, err)` on failure.

**Example:**

```lua
local model, err = maki.agent.resolve_model(ctx, { tier = "fast" })
if err then error(err) end
print(model.spec, model.tier)
```

---

### `maki.agent.system_prompt()` {#maki-agent-system_prompt}

```lua
maki.agent.system_prompt({ctx}, {opts})
```

Build a system prompt from the parent's resolved prompt catalogue.
Plugin template and section replacements apply. Environment variables like
`{cwd}` are substituted automatically. Use this when you need a ready-made
prompt for a subagent session.

**Parameters:**

- `{ctx}` (`LuaCtx`) Agent context.
- `{opts}` (`table`) Required fields:
  - `prompt_id` (`string`) one of `"research"`, `"general"`, `"system"`.

  Optional fields:

  - `instructions` (`string|boolean?`) text inserted at the template's
    `{{instructions}}` marker.
    `true` loads project instruction files.
    `false` or nil omits them.

**Returns:** (`string?`, `string?`) The assembled prompt string, or `(nil, err)` on failure.

**Example:**

```lua
local prompt, err = maki.agent.system_prompt(ctx, {
  prompt_id = "research",
  instructions = true,
})
if err then error(err) end
```

---

### `maki.agent.tools()` {#maki-agent-tools}

```lua
maki.agent.tools({ctx}, {opts?})
```

Get the list of tool definitions for a given audience. Pass the result
straight into `maki.agent.session()` or use it to inspect what tools are
available.

Without a `ctx`, pass `session = id` in `opts` to get the tools a
background session of that chat would have.

**Parameters:**

- `{ctx}` (`LuaCtx|table`) Agent context, or `opts` when called without one.
- `{opts?}` (`table?`) Required with a ctx. Fields:
  - `session` (`string`) only without a ctx: id of a live chat session.
  - `audience` (`string`) tool audience filter, e.g. `"general"`, `"subagent"`,
    `"general_sub"`.

  Optional fields:

  - `only` (`string[]?`) include only these tool names.
  - `except` (`string[]?`) exclude these tool names.
  - `workflow` (`boolean?`) use workflow-mode descriptions. Default: `false`.
  - `spec` (`string?`) evaluate capability exclusions against this model spec.
  - `mcp` (`boolean?`) describe tools as if MCP is reachable. Default: `false`.
    Match what you pass to `maki.agent.session()`, or the descriptions
    advertise MCP tools the session cannot call.

**Returns:** (`table?`, `string?`) Array of tool definition tables, or `(nil, err)` on failure.

**Example:**

```lua
local defs, err = maki.agent.tools(ctx, {
  audience = "general_sub",
  except = { "bash", "write" },
})
if err then error(err) end
print(#defs .. " tools available")
```

---

### `maki.agent.callable_tools()` {#maki-agent-callable_tools}

```lua
maki.agent.callable_tools({ctx})
```

Every tool name this context can dispatch: registry tools, MCP tools
(deferred ones included), host tools (ACP client tools, a subagent's
`structured_output`) and `tool_search`. Reach for it when you expose tools
inside a sandbox and need the names to bind. `maki.api.get_tools()` covers
the registry alone and has no view of the session.

The list already accounts for this session's audience, the config's
`disabled_tools` and the model's capabilities. Read `audiences` to layer
your own policy on top. A sandbox wants `interpreter`.

Each name shows up once, described by the tool a call would really reach, so
a host tool that shadows a registry name reports its own audience rather
than the shadowed one's.

**Parameters:**

- `{ctx}` (`LuaCtx`) Agent context.

**Returns:** (`table?`, `string?`) Array of `{ name, alias?, source, audiences, schema? }`,
  or `(nil, err)` on failure. `source` is one of `"native"`, `"local"`,
  `"mcp"`. `alias` is a safe identifier to bind, set only when `name` is not
  one (say `srv__get-docs`). Dispatch `name` in every case. `schema` comes
  with registry tools only.

**Example:**

```lua
local tools, err = maki.agent.callable_tools(ctx)
if err then error(err) end
for _, t in ipairs(tools) do
  print(t.source, t.alias or t.name)
end
```

---

### `maki.agent.call_tool()` {#maki-agent-call_tool}

```lua
maki.agent.call_tool({ctx}, {name}, {input}, {opts?})
```

Run a tool by name and wait for the result. This is how you call built-in
tools (like `read`, `bash`, `glob`) from Lua without going through the LLM.

Live events (streaming output, annotations, cumulative usage) are delivered
through optional callbacks while the tool runs.

**Parameters:**

- `{ctx}` (`LuaCtx`) Agent context.
- `{name}` (`string`) Tool name, e.g. `"bash"`, `"read"`.
- `{input}` (`table|any`) Tool input (JSON-serializable). Must match the tool's `input_schema`.
- `{opts?}` (`table?`) Optional fields:
  - `timeout` (`integer?`) deadline in seconds.
  - `output_mode` (`string?`) "full" (default) returns the complete text.
    "compact" returns what the model would get, with output limits and
    RTK rewriting. Permissions and hooks apply in both modes, and hooks
    see the mode as `ctx.output_mode`.
  - `on_live_buf` (`function?`) called with a `BufHandle` for each live buffer
    the tool publishes. Must not yield.
  - `on_annotation` (`function?`) called with an annotation string for each
    annotation event. Must not yield. A call a `permission.prompt` layer
    allowed arrives as `"allowed by <plugin>"`. Without this callback that
    mark goes on your own tool's row instead.
  - `on_usage` (`function?`) called with a formatted cumulative token usage
    string. Must not yield.

**Returns:** (`string?`, `string?`) Tool output text, or `(nil, err)` on failure.
  Instruction files the child picks up (a subdirectory `AGENTS.md`) are
  not in the text: they land on the calling tool's own result.

**Example:**

```lua
local out, err = maki.agent.call_tool(ctx, "bash", {
  command = "ls -la",
  timeout = 10,
})
if err then error(err) end
print(out)
```

---

### `maki.agent.session()` {#maki-agent-session}

```lua
maki.agent.session({ctx}, {opts?})
```

Create a new subagent session. The session inherits the parent model and
MCP handle unless you override them. You get back a `Session` object that
you can send messages to with `:prompt()`.

This is the main way to spin up a sub-conversation with its own history
and tool set.

Without a `ctx`, `maki.agent.session({ session = id, ... })` opens a
background session. It borrows the model, permissions and MCP of the live
chat session `id` and is not tied to any turn. Call it and its `:prompt()`
from a task: a `maki.async.spawn` task, an autocmd callback or a tool
handler. This form needs the `agent` plugin permission.

Nobody can answer its permission prompts, so a call that would prompt is
denied and the subagent is told why. Your rules, "allow for this session"
answers and YOLO still apply, so add allow rules for what it runs (see
[Permissions](/docs/permissions/)). It always runs in build mode. While the
chat is in plan mode its file-write tools are denied. Its thinking never
goes above the chat's current setting.

With or without a ctx, the plugin must hold what its tools reach. A prompt
is no gate under YOLO or an allow rule, and a background session has
nobody to prompt. A tool declaring a permission (`bash` with `run`, `read`
with `fs_read`) costs that one. A tool declaring none (`batch`,
`code_execution`), or `mcp = true` on a chat with MCP servers, costs every
permission except `agent`. `local_tools` cost nothing, they run as the
plugin. The bundled `task` hands over the whole chat, so it holds every
permission.

`scope` sets the lifetime. `"task"`, the default, ends the session with
the task that opened it: an autocmd callback when it returns, a tool
handler with its tool call, a `maki.async.spawn` task when it finishes.
`"session"` outlives the task. Both end at `:close()`, when the chat
session ends (`/new`, closing the tab) and when the plugin unloads
(`/reload`). Turn ends, Esc and rewinds leave it alone. Esc on a tool
call still cancels a prompt that the tool's handler is waiting on.

A polling monitor opens one per round inside a `maki.async.spawn` loop.
An event-driven one opens once with `scope = "session"`, keeps the handle
and prompts it from autocmd callbacks.

Opened by chat session id, it shows up in that chat as a task owned by
the plugin as soon as it opens: a transcript tab, a `plugin` field in
`/tasks` and `maki.task.list()`, `"idle"` before and between prompts, and
its spend in the chat's totals after each turn. Esc in its tab or Ctrl+S in
`/tasks` closes it, and the pending `:prompt()` returns `"stopped by user"`
rather than `"cancelled"`, so a loop can tell the user's stop from its own
task ending and stop rather than reopen. Permission prompts stay denied even
though the task has a tab: a prompt in an unfocused tab would stall a
monitor silently, a denial reaches the plugin as an error it can handle.
Setting `plugins.<name>.enabled = false` stops a plugin's monitors from
the next `/reload`. Under `maki -p` and ACP there is no live chat
session, so opening one fails with "session not live". The
[maki.agent](#maki-agent) overview has a full monitor.

**Parameters:**

- `{ctx}` (`LuaCtx|table`) Agent context, or `opts` when called without one.
- `{opts?}` (`table?`) Required with a ctx. Optional fields:
  - `session` (`string`) required without a ctx: id of the live chat session
    that owns it. Returns `(nil, "session not live: <id>")` once that
    session has ended.
  - `scope` (`string?`) only without a ctx: `"task"` (default) or
    `"session"`, see above.
  - `model_spec` (`string?`) model spec string to use instead of the parent model.
  - `system` (`string?`) system prompt. Defaults to empty.
  - `tools` (`table?`) tool definitions array (from `maki.agent.tools()`).
  - `local_tools` (`table?`) map of `name -> spec` for Lua-backed tools. Each spec
    requires `description` (string), `input_schema` (table), and
    `handler` (function). The handler receives the input table and must return
    `(string)` or `(nil, err)`. Optional `audiences` (string[]) gates who may
    call it, the same way `maki.api.register_tool` does. The default is the
    model alone, so a script cannot reach it through `code_execution`.
  - `name` (`string?`) display name for logs and UI.
  - `audience` (`string?`) tool audience for capability gating. Default: `"general_sub"`.
  - `mcp` (`boolean?`) give the session MCP tools. Their definitions are
    injected each turn (deferred behind `tool_search`), so do not put MCP
    definitions in `tools`. Default: `false`.
  - `thinking` (`string|integer?`) thinking mode: `"off"`, `"adaptive"`, an
    effort level (`"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`,
    `"max"`), or a budget integer (token count). Inherits the parent
    setting if omitted, and is capped at it otherwise.
  - `fast` (`boolean?`) use fast mode. Inherits parent setting if omitted.

**Returns:** ([`Session?`](#maki-agent-Session), `string?`) Session handle, or `(nil, err)` on failure.

**Example:**

```lua
local tools = maki.agent.tools(ctx, { audience = "general_sub" })
local sess, err = maki.agent.session(ctx, {
  system = "You are a research assistant.",
  tools = tools,
  name = "researcher",
})
if err then error(err) end

-- Close before handling the error, so no path leaves the session open.
local result, prompt_err = sess:prompt("Summarize this file.")
sess:close()
if prompt_err then error(prompt_err) end
```

---

### `maki.agent.list()` {#maki-agent-list}

```lua
maki.agent.list({opts})
```

List the sessions plugins opened on a chat session without a `ctx`, see
[maki.agent.session](#maki-agent-session).

Each entry has these fields:
- `id` (`string`) Session id.
- `session` (`string`) Chat session it belongs to.
- `plugin` (`string`) Plugin that opened it.
- `name` (`string`) Name given at open.
- `status` (`string`) `"working"` during a `:prompt()`, otherwise `"idle"`.
- `started_at` (`integer`) Unix seconds.
- `cost` (`number?`) Dollars billed so far, updated after each turn. nil until
  a priced turn ends.

**Parameters:**

- `{opts}` (`table`) Required fields:
  - `session` (`string`) id of the chat session.

**Returns:** (`table?`, `string?`) Array of entries, or `(nil, err)` on failure.

**Example:**

```lua
for _, s in ipairs(maki.agent.list({ session = id })) do
  print(s.plugin, s.name, s.status, os.time() - s.started_at)
end
```


## maki.agent.Session {#maki-agent-Session}

A subagent session with its own conversation history.

Create one with `maki.agent.session()`, then send messages with
`:prompt()`. The session remembers previous turns, so you can have
a multi-step conversation. `:send()` nudges a prompt that is still
running.

Always call `:close()` when you are done, on error paths too. The
garbage collector is a fallback that may never run while the VM sits
idle, so a session you only drop can stay open for the rest of the run.

---

### `Session:prompt()` {#Session-prompt}

```lua
Session:prompt({message})
```

Send a message to the subagent and wait for its full response. The agent
loop runs to completion, calling tools as needed. Conversation history is
kept across calls, so you can have a multi-turn conversation.

The returned table has fields: `text` (string), `duration_ms` (integer),
`input_tokens` (integer), `output_tokens` (integer). `text` is an empty
string when the subagent produced no text block (e.g. it only called
tools).

**Parameters:**

- `{message}` (`string`) User message to send.

**Returns:** (`table?`, `string?`) Result table on success, or `(nil, err)` on
failure. A run cut short after streaming some text hands you both: the
error and a `{ text = <what it streamed> }` table. `"cancelled"` means the
task that opened or prompted the session ended. `"stopped by user"` means
Esc in its tab closed a session opened by chat session id, and every
later prompt answers the same.

**Example:**

```lua
local r, err = sess:prompt("What files are in this project?")
if err then error(err) end
print(r.text)
print(r.input_tokens .. " input, " .. r.output_tokens .. " output tokens")
```

---

### `Session:send()` {#Session-send}

```lua
Session:send({message})
```

Hand the subagent a message without waiting, the way a user typing in its
tab does. A running `:prompt()` picks it up at its next turn boundary, once
the current reply and its tool calls finish, and the model reads it as a
user interrupt. The tab draws it when it lands. Messages sent in a burst
land together.

With no prompt running, or one already past its last turn, the message
waits. The next `:prompt()` sends it in the same request as its own
message, just before it, so the first reply already reads it.

**Parameters:**

- `{message}` (`string`) Message to deliver.

**Returns:** (`boolean?`, `string?`) `true`, or `(nil, err)` when the session takes
no more messages. The errors are the ones `:prompt()` refuses with. `true`
means queued, not delivered: a message still waiting when the session
closes is dropped.

**Example:**

```lua
-- From another task, while `sess:prompt()` is still running.
local ok, err = sess:send("New finding: the bug is in parser.rs, focus there.")
if not ok then maki.log.warn(err) end
```

---

### `Session:close()` {#Session-close}

```lua
Session:close()
```

Close the session and flush its history back to the parent agent. Calling
it more than once is safe.

Close on every path, error paths included. Dropping the session instead
leaves the work to the Lua garbage collector, which may never run while
the VM sits idle, and the subagent's event relay stays alive until it does.

Close cancels a running prompt before waiting to flush its history. That
prompt returns a cancelled error and any partial text.

---

### `Session:id()` {#Session-id}

```lua
Session:id()
```

The task id of this session. A session opened by chat session id goes by
this id in `maki.task.list()` and the `"TaskStopped"` event, so a plugin
can tell which of those are its own.

**Returns:** (`string`) Session task id.


## maki.async {#maki-async}

Tools for running things concurrently in Lua plugins.

`run` starts a background task that ends with its caller, and `spawn`
one that lives as long as the plugin. `gather` and `join` run several
functions at once, and `semaphore` limits how many. `await` and `wrap`
turn callback APIs into coroutine calls.

```lua
local results = maki.async.gather({
  function() return fetch("a.txt") end,
  function() return fetch("b.txt") end,
})
```

---

### `maki.async.run()` {#maki-async-run}

```lua
maki.async.run({fn}, {on_finish?})
```

Start {fn} as a background task without waiting for it. Pass
{on_finish} to get the result. The task is cancelled with its caller
and stopped after 60 seconds. For work that outlives the caller, use
`maki.async.spawn`.

**Parameters:**

- `{fn}` (`function`) Zero-argument function to execute.
- `{on_finish?}` (`function?`) Optional callback `function(err, result)`. Called once {fn} completes.

**Example:**

```lua
maki.async.run(function()
  local data = expensive_fetch()
  process(data)
end)
```

---

### `maki.async.spawn()` {#maki-async-spawn}

```lua
maki.async.spawn({fn})
```

Run {fn} in a task that lives as long as your plugin, such as a
repeating timer or a connection opened at load.

The task has no deadline and outlives the call that started it, so a
tool handler can spawn it and return. It ends when {fn} returns or
raises, when you call `task:cancel()`, or when the plugin unloads.
Errors are logged and flashed with the plugin name.

Spawned at the top level of a plugin file, it starts after the plugin
loads. It does not keep maki alive: `maki -p` drops it on exit.

Code that runs 5 seconds without yielding is still stopped. See
`maki.async.sleep`.

**Parameters:**

- `{fn}` (`function`) Zero-argument function to run.

**Returns:** ([`maki.async.Task`](#maki-async-Task)) Handle with `:cancel()`.

**Example:**

```lua
local task = maki.async.spawn(function()
  while true do
    maki.async.sleep(2000)
    report()
  end
end)

task:cancel()
```

---

### `maki.async.sleep()` {#maki-async-sleep}

```lua
maki.async.sleep({ms})
```

Suspend the calling task for {ms} milliseconds. Other tasks and the UI
keep running. Cancelling the task ends the sleep early with an error.

All plugins share one Lua thread. Code that runs for 5 seconds without
yielding is stopped with an error. `sleep(0)` lets every other ready
task run once, then returns. Call it periodically in long loops.

A repeating timer is a sleep loop inside `maki.async.spawn`. For a
one-shot timer that outlives the tool call, such as a toast that
dismisses itself, use `maki.defer_fn`.

**Parameters:**

- `{ms}` (`integer`) Milliseconds to wait. Negative values raise an error.

**Example:**

```lua
maki.async.spawn(function()
  while true do
    poll()
    maki.async.sleep(5000)
  end
end)

-- A long loop that keeps the rest of maki responsive:
for i, line in ipairs(lines) do
  if i % 1000 == 0 then
    maki.async.sleep(0)
  end
  process(line)
end
```

---

### `maki.async.await()` {#maki-async-await}

```lua
maki.async.await({argc}, {fn}, {...})
```

Turn a callback-based function into a normal call you can use in a coroutine. It calls `fn(..., callback)`, inserting the callback at position {argc}, then suspends your coroutine until the callback fires. You get back whatever the callback was called with.

**Parameters:**

- `{argc}` (`integer`) Total number of positional arguments {fn} expects (including the callback). Must be >= 1.
- `{fn}` (`function`) Callback-based function to call.
- `{...}` (`any`) Extra arguments forwarded to {fn} before the injected callback.

**Returns:** (`...`) Values passed by the caller to the injected callback.

**Example:**

```lua
local result = maki.async.await(2, http.get, url)
```

---

### `maki.async.wrap()` {#maki-async-wrap}

```lua
maki.async.wrap({argc}, {fn})
```

Create a coroutine-friendly wrapper around a callback-based function. The wrapper calls `maki.async.await` for you, so you can use the result like a normal function call.

**Parameters:**

- `{argc}` (`integer`) Callback position, forwarded to `maki.async.await`.
- `{fn}` (`function`) Callback-based function to wrap.

**Returns:** (`function`) Wrapped function you can call like a normal function.

**Example:**

```lua
local get = maki.async.wrap(2, http.get)
local body = get(url)
```

---

### `maki.async.join()` {#maki-async-join}

```lua
maki.async.join({max_jobs}, {fns})
```

Run all functions in {fns} with at most {max_jobs} going at once. Waits until every function has finished. Unlike `gather`, this does not return individual results.

**Parameters:**

- `{max_jobs}` (`integer`) Maximum number of functions running at the same time.
- `{fns}` (`table`) Array of zero-argument functions to execute.

**Example:**

```lua
maki.async.join(4, {
  function() process(files[1]) end,
  function() process(files[2]) end,
  function() process(files[3]) end,
})
```

---

### `maki.async.gather()` {#maki-async-gather}

```lua
maki.async.gather({fns})
```

Run all functions in {fns} at the same time and collect their results.
Unlike `join`, this gives you back the return value (or error) from each
function. The results are in the same order as the input.

Each entry in the result array has `ok` (boolean), and either `value`
(on success) or `err` (string, on failure).

**Parameters:**

- `{fns}` (`table`) Array of zero-argument functions.

**Returns:** (`table`) Array of result tables, one per function.

**Example:**

```lua
local results = maki.async.gather({
  function() return fetch("a.txt") end,
  function() return fetch("b.txt") end,
})
for i, r in ipairs(results) do
  if r.ok then print(r.value) else print("error: " .. r.err) end
end
```

---

### `maki.async.semaphore()` {#maki-async-semaphore}

```lua
maki.async.semaphore({n})
```

Create a counting semaphore that allows at most {n} concurrent permits.
Use this to limit how many tasks hit a resource at the same time.

**Parameters:**

- `{n}` (`integer`) Maximum number of concurrent permits. Values below 1 are clamped to 1.

**Returns:** ([`maki.async.Semaphore`](#maki-async-Semaphore)) A new semaphore.

**Example:**

```lua
local sem = maki.async.semaphore(5)
-- each task acquires a permit before doing work
local permit = sem:acquire()
do_work()
permit:release()
```

---

### `maki.async.on_cancel()` {#maki-async-on_cancel}

```lua
maki.async.on_cancel({fn})
```

Register {fn} to run as soon as the current task is cancelled or hits
its deadline, without waiting for whatever it is doing to finish. Use
it to paint the cancelled state: a handler waiting on children
(`gather`, `call_tool`) stays parked until they wind down, so anything
after the wait is too late to reach the screen.

The callback receives the reason (`"cancelled"` or `"timeout"`) and may
still call `ctx:finish`; the host prefers that reply over the generic
cancelled/timeout error. Mark it `is_error = true` and end it with a
marker, so the model knows the output it gets is cut short.

The callback runs outside your coroutine, so it must not yield. It
fires at most once, immediately if the task is already cancelled. An
error inside it is logged and never reaches your handler, and the
other hooks still run.

**Parameters:**

- `{fn}` (`function`) Function to run on cancel; receives the reason string.

**Example:**

```lua
maki.async.on_cancel(function(reason)
  view:append({ { reason, "tool_error" } })
  ctx:finish({ llm_output = partial .. "\n[cancelled; output is partial]", is_error = true })
end)
maki.async.gather(children)
```


## maki.async.Semaphore {#maki-async-Semaphore}

A counting semaphore for limiting how many tasks run at once.

Create one with `maki.async.semaphore(n)`, then call `:acquire()` to
get a permit before doing work. If the task is cancelled, the acquire
is cancelled too.

---

### `Semaphore:acquire()` {#Semaphore-acquire}

```lua
Semaphore:acquire()
```

Wait for a permit from the semaphore. Your coroutine suspends until a slot
opens up. If the owning task is cancelled, the acquire is cancelled too.

**Returns:** ([`maki.async.Permit`](#maki-async-Permit)) A permit handle. Call `:release()` when done, or let it be garbage collected.

**Example:**

```lua
local sem = maki.async.semaphore(3)
local permit = sem:acquire()
-- do work that needs the slot
permit:release()
```


## maki.async.Permit {#maki-async-Permit}

One slot in a semaphore, obtained from `Semaphore:acquire()`.

The slot is held until you call `:release()` or until the permit is
garbage collected. Releasing early lets other tasks acquire sooner.

---

### `Permit:release()` {#Permit-release}

```lua
Permit:release()
```

Give the permit back to the semaphore so another task can acquire it.
Throws if you already released this permit.


## maki.async.Task {#maki-async-Task}

Handle returned by `maki.async.spawn`.

---

### `Task:cancel()` {#Task-cancel}

```lua
Task:cancel()
```

Stop the task. A task that is waiting ends right away and runs its
`maki.async.on_cancel` hooks. A task that cancels itself stops at its
next yield. Extra calls do nothing.


## maki.base64 {#maki-base64}

Base64 encoding and decoding, modelled after `vim.base64`.

Both functions accept strings and Luau buffers, so you can round-trip
binary data read with `maki.fs.read_bytes`.

```lua
local encoded = maki.base64.encode("hello")
local decoded = maki.base64.decode(encoded)
```

---

### `maki.base64.encode()` {#maki-base64-encode}

```lua
maki.base64.encode({data})
```

Encode {data} to standard Base64. Like `vim.base64.encode`.
Accepts both strings and Luau buffers.

**Parameters:**

- `{data}` (`string|buffer`) Data to encode.

**Returns:** (`string`) Base64-encoded string.

**Example:**

```lua
maki.base64.encode("hello") -- "aGVsbG8="
```

---

### `maki.base64.decode()` {#maki-base64-decode}

```lua
maki.base64.decode({str})
```

Decode a Base64-encoded {str} back to its original bytes. Like `vim.base64.decode`.

**Parameters:**

- `{str}` (`string|buffer`) Base64-encoded text.

**Returns:** (`string?`, `string?`) Decoded bytes as a string, or nil plus an error
  message if {str} is not valid Base64.

**Example:**

```lua
maki.base64.decode("aGVsbG8=") -- "hello"
```


## maki.env {#maki-env}

Paths to maki's own directories (config, state, logs, legacy).

Use these to locate config files or persistent state without hard-coding paths.

These answer where maki keeps its files, so they need `fs_read`, which a
plugin needs to read anything there anyway. Asking for a path must not
cost a plugin `env`, which covers the process environment alone
(`maki.uv.os_getenv`), where secrets live.

```lua
local cfg = maki.env.config_dir()
```

---

### `maki.env.state_dir()` {#maki-env-state_dir}

```lua
maki.env.state_dir()
```

Return the directory where maki stores runtime state (sessions, auth tokens, etc.).
Typically something like `~/.local/state/maki`.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Returns:** (`string?`) State directory path, or nil if it cannot be determined.

**Example:**

```lua
local dir = maki.env.state_dir()
```

---

### `maki.env.config_dir()` {#maki-env-config_dir}

```lua
maki.env.config_dir()
```

Return the directory where maki looks for user configuration files.
Typically something like `~/.config/maki`.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Returns:** (`string?`) Config directory path, or nil if it cannot be determined.

**Example:**

```lua
local dir = maki.env.config_dir()
```

---

### `maki.env.logs_dir()` {#maki-env-logs_dir}

```lua
maki.env.logs_dir()
```

Return the directory where maki writes its log files (`maki.log`).
Typically something like `~/.local/logs/maki`.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Returns:** (`string?`) Logs directory path, or nil if it cannot be determined.

**Example:**

```lua
local dir = maki.env.logs_dir()
```

---

### `maki.env.legacy_dir()` {#maki-env-legacy_dir}

```lua
maki.env.legacy_dir()
```

Return the legacy config path (`~/.maki`), if it exists on disk.
Useful for migration logic. Returns nil when there is no legacy directory.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Returns:** (`string?`) Legacy directory path, or nil if not present.


## maki.fn {#maki-fn}

Process and environment helpers, modeled after Neovim's `vim.fn` job
control. Use these to run shell commands, wait for output, and check
whether programs are installed.

```lua
local id = maki.fn.jobstart("git status", {
  on_exit = function(_, code) print("done: " .. code) end,
})
```

---

### `maki.fn.jobstart()` {#maki-fn-jobstart}

```lua
maki.fn.jobstart({cmd}, {opts?})
```

Run a command in the background. A string runs through `bash -c` on Unix
and through [`agent.shell`](/docs/configuration/#agent) on Windows. A table
is spawned as argv with no shell, so nothing in it is read as a redirect, a
pipe, or `$(...)`.
You get back a job id that you can pass to `jobstop` or `jobwait` to
control the process.

`stdout` and `stderr` route a stream to a file instead of into maki. A
path is opened for append and handed to the child, so nothing is buffered
here: no callback, no tail, no events for that stream, and it counts as
truncated everywhere a tail is reported. That makes the two mutually
exclusive with `on_stdout` / `on_stderr` for the same stream, and a path
additionally needs the `fs_write` permission. To both persist and react,
run one job writing the file and a second one tailing it.

Requires the `run` [plugin permission](#plugin-permissions).

**Parameters:**

- `{cmd}` (`string|table`) Shell command, or an argv table like

  `{ "tail", "-F", path }`.

- `{opts?}` (`table?`) Optional settings:
  - `cwd` (`string?`) working directory (tilde is expanded).
  - `env` (`table?`) extra environment variables, `{ VAR = "value" }`.
  - `on_stdout` (`function?`) called with `(job_id, line)` for each stdout line.
  - `on_stderr` (`function?`) called with `(job_id, line)` for each stderr line.
  - `on_exit` (`function?`) called with `(job_id, code)` when the process finishes.
  - `stdout` (`string|false?`) append stdout to this path, or `false` to
    discard it.
  - `stderr` (`string|false?`) same for stderr; both may name one path.
  - `scope` (`string|table?`) job lifetime. `"task"` (default) ends the job
    with the current call. `"plugin"` keeps it alive until the plugin
    unloads or reloads. `{ session = "<id>" }` keeps it alive until that
    session ends, and survives plugin reload.
  - `tail` (`integer?`) trailing lines per stream kept for `jobinfo`
    (default 20, 0 disables, max 1024).
  - `name` (`string?`) handle for `jobfind`, unique among the live jobs this
    plugin can see. Starting a second job under a live name is an error.

**Returns:** (`integer?`, `string?`) Job id, or nil plus an error message when the
  process could not start (binary not found, bad `cwd`, redirect file not
  writable).

**Example:**

```lua
local id, err = maki.fn.jobstart({ "rg", "--json", pattern, dir }, {
  on_stdout = function(_, line) print(line) end,
  on_exit = function(_, code) print("exit: " .. code) end,
})
if not id then
  maki.log.warn("rg failed to start: " .. err)
end
```

---

### `maki.fn.jobstop()` {#maki-fn-jobstop}

```lua
maki.fn.jobstop({job_id})
```

Kill a running job immediately (SIGKILL on Unix). Safe to call on
jobs that already exited or on unknown ids.

Requires the `run` [plugin permission](#plugin-permissions).

**Parameters:**

- `{job_id}` (`integer`) Job id returned by `jobstart`.

**Example:**

```lua
maki.fn.jobstop(id)
```

---

### `maki.fn.jobforget()` {#maki-fn-jobforget}

```lua
maki.fn.jobforget({job_id})
```

Drop an exited session-owned job from the store. Running jobs are left
alone; use `jobstop` to kill those. Unknown ids are a no-op.

Requires the `run` [plugin permission](#plugin-permissions).

**Parameters:**

- `{job_id}` (`integer`) Job id returned by `jobstart`.

**Example:**

```lua
maki.fn.jobforget(id)
```

---

### `maki.fn.jobwait()` {#maki-fn-jobwait}

```lua
maki.fn.jobwait({job_id}, {timeout_ms?})
```

Wait for a job to finish and collect its output. Returns a result
table with `stdout`, `stderr`, `exit_code`, and `truncated`. A job that
already exited answers from its captured tail, so `truncated` says
whether that tail ever lost a line (`tail` too small or 0, or the stream
redirected away). Waiting on a live job collects every line and is never
truncated.

While waiting, the job's `on_stdout`, `on_stderr`, and `on_exit`
callbacks fire as events arrive (like Neovim), so you can stream
output into a buffer while parked here. An already-exited
session-owned job answers from its snapshot and fires no callbacks.
Task and plugin jobs leave the store on exit, so waiting after that
answers nil plus an error.

Requires the `run` [plugin permission](#plugin-permissions).

**Parameters:**

- `{job_id}` (`integer`) Job id returned by `jobstart`.
- `{timeout_ms?}` (`integer?`) Maximum wait in milliseconds (default 30000).

**Returns:** (`table?`, `string?`) `{ stdout, stderr, exit_code, truncated }`, or
  nil plus an error on timeout or an unknown job.

**Example:**

```lua
local id = maki.fn.jobstart("echo hello")
local result, err = maki.fn.jobwait(id, 5000)
if result then
  print(result.stdout)
end
```

---

### `maki.fn.jobinfo()` {#maki-fn-jobinfo}

```lua
maki.fn.jobinfo({job_id})
```

Snapshot a job this plugin can see. Live jobs report tails collected
so far; session-owned jobs still answer after they exit.

Requires the `run` [plugin permission](#plugin-permissions).

**Parameters:**

- `{job_id}` (`integer`) Job id returned by `jobstart`.

**Returns:** (`table|nil`, `string|nil`) `{ id, command, name, pid, session, status,
  exit_code, elapsed_secs, stdout_lines, stderr_lines }`, or nil and
  an error. `status` is `"running"` or `"exited"`.

**Example:**

```lua
local info = maki.fn.jobinfo(id)
```

---

### `maki.fn.joblist()` {#maki-fn-joblist}

```lua
maki.fn.joblist({session?})
```

List jobs this plugin can see, including exited session-owned jobs (so an
id started before a reload stays findable). Rows identify the job; call
`jobinfo` for tails. Pass a session id to list only that session's jobs.
Plugin and task jobs carry no session, so a filter never matches them.

Requires the `run` [plugin permission](#plugin-permissions).

**Parameters:**

- `{session?}` (`string?`) Session id filter.

**Returns:** (`table`) array of `{ id, command, name, pid, session, status,
  exit_code, elapsed_secs }`.

**Example:**

```lua
local jobs = maki.fn.joblist(maki.session.current())
```

---

### `maki.fn.jobattach()` {#maki-fn-jobattach}

```lua
maki.fn.jobattach({job_id}, {opts})
```

Attach (or replace) callbacks on a job this plugin can see. This is how a
plugin picks its jobs back up after a reload: unloading drops the Lua
callbacks of its session-owned jobs, but the processes keep running.

Keys absent from {opts} leave the current callback alone. Attaching
`on_exit` to a job that already exited still fires it once, with the
recorded exit code, so a reload racing the exit cannot lose it.

Requires the `run` [plugin permission](#plugin-permissions).

**Parameters:**

- `{job_id}` (`integer`) Job id, e.g. from `joblist`.
- `{opts}` (`table`) `on_stdout`, `on_stderr`, `on_exit`: a function, or `false` to clear.

**Returns:** (`boolean|nil`, `string|nil`) true on success, or nil and an error.

**Example:**

```lua
-- A monitor that survives /reload: adopt the live job or start one.
local sid = maki.session.current()
local id = maki.fn.jobfind("log-tail")
  or maki.fn.jobstart({ "tail", "-F", path }, {
    name = "log-tail",
    scope = { session = sid },
  })
maki.fn.jobattach(id, {
  on_stdout = function(_, line) maki.session.notify(line, { session = sid }) end,
  on_exit = function(_, code) maki.session.notify("tail died: " .. code, { session = sid }) end,
})
```

---

### `maki.fn.jobfind()` {#maki-fn-jobfind}

```lua
maki.fn.jobfind({name})
```

Find the live job of this plugin that `jobstart` registered under {name}.
An exited job never answers, so `jobfind(...) or jobstart(...)` restarts a
job that died instead of adopting its id. The name stays on the `joblist`
row, which is where you go to see why it died.

Requires the `run` [plugin permission](#plugin-permissions).

**Parameters:**

- `{name}` (`string`) Name passed to `jobstart`.

**Returns:** (`integer|nil`, `string|nil`) Job id, or nil and an error when no live
  job holds the name.

**Example:**

```lua
local id = maki.fn.jobfind("log-tail")
if not id then
  id = maki.fn.jobstart("tail -F /tmp/log", { name = "log-tail", scope = "plugin" })
end
```

---

### `maki.fn.executable()` {#maki-fn-executable}

```lua
maki.fn.executable({name})
```

Check whether {name} can be found on `$PATH` or is an absolute path
to a file. Returns 1 when found, 0 otherwise (matches Neovim's
`vim.fn.executable`).

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{name}` (`string`) Program name (e.g. `"git"`) or absolute path.

**Returns:** (`integer`) `1` if found, `0` otherwise.

**Example:**

```lua
if maki.fn.executable("rg") == 1 then
  -- use ripgrep
end
```

---

### `maki.fn.winsaveview()` {#maki-fn-winsaveview}

```lua
maki.fn.winsaveview()
```

Read the viewport of the focused chat transcript, like Neovim's
`vim.fn.winsaveview()`. The transcript is the only scrollable window
maki has, so there is no window argument.

`topline` is the 1-based transcript line at the top of the viewport, so
the last visible one is `math.min(topline + height - 1, line_count)`.
`auto_scroll` has no Vim counterpart: it is true while the transcript
follows streaming output.

**Returns:** (`table|nil`, `string|nil`) `{topline, line_count, height, auto_scroll}`, or nil and an error.

**Example:**

```lua
local view = maki.fn.winsaveview()
maki.fn.winrestview({ topline = view.topline + 1 })
```

---

### `maki.fn.winrestview()` {#maki-fn-winrestview}

```lua
maki.fn.winrestview({view})
```

Scroll the focused chat transcript so that the `topline` field of
{view} becomes the top visible line, like Neovim's
`vim.fn.winrestview()`. Out of range values are clamped. Other keys are
ignored, so a table straight from `winsaveview()` round-trips.

Scrolling away from the bottom unpins the transcript; landing back at
the bottom re-pins it so streaming output keeps following.

**Parameters:**

- `{view}` (`table`) View to restore. Only `topline` (1-based) is read.

**Returns:** (`boolean|nil`, `string|nil`) true on success, or nil and an error.

**Example:**

```lua
maki.fn.winrestview({ topline = 1 })
```

---

### `maki.fn.mode()` {#maki-fn-mode}

```lua
maki.fn.mode()
```

The current mode, like `vim.fn.mode()`: `"i"` while the chat input has
the keyboard and takes typed text, `"n"` otherwise. See
[Modes](/docs/lua-api/#modes). Always `"n"` without a UI.

**Returns:** (`string`) `"n"` or `"i"`.

**Example:**

```lua
maki.keymap.set({ "n", "i" }, "<M-m>", function()
  maki.ui.set_mode(maki.fn.mode() == "n" and "i" or "n")
end)
```


## maki.fs {#maki-fs}

File-system utilities, modelled after `vim.fs` and `vim.uv`.

Fallible operations return `(value, err)` pairs and never throw.
Paths support `~/` expansion. Relative paths resolve from the current working directory.

```lua
local text, err = maki.fs.read("init.lua")
if err then return end
```

---

### `maki.fs.read()` {#maki-fs-read}

```lua
maki.fs.read({path}, {opts?})
```

Read the file at {path} as a UTF-8 string. Pass {opts} to read only some
lines, such as the end of a log.
Returns nil plus an error message if the text read is over 64 MiB or not
valid UTF-8. Skipped lines do not count, so tailing a huge log works.
Use `read_bytes` for binary files.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{path}` (`string`) Absolute or relative file path. `~/` is expanded to the home directory.
- `{opts?}` (`table?`) `offset` is the 1-based line to start at (default 1). A negative value counts back from the end, so `-50` reads the last 50 lines. An offset past the end returns an empty string. `limit` is the max number of lines, `0` or omitted reads to the end.

**Returns:** (`string?`, `string?`) File contents, or nil plus an error message.

**Example:**

```lua
local text, err = maki.fs.read("config.toml")
if err then
  maki.log.warn("could not read config: " .. err)
  return
end
local tail = maki.fs.read("server.log", { offset = -50 })
```

---

### `maki.fs.read_bytes()` {#maki-fs-read_bytes}

```lua
maki.fs.read_bytes({path}, {opts?})
```

Read the file at {path} as raw bytes in a Luau buffer. Pass {opts} to read
a byte range.
Returns nil plus an error message if the bytes read are over 64 MiB.
Use it for binary files or data headed for `maki.base64.encode`.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{path}` (`string`) Absolute or relative file path. `~/` is expanded to the home directory.
- `{opts?}` (`table?`) `offset` is the byte to start at (default 0). A negative value counts back from the end, so `-1024` reads the last 1024 bytes. `len` is the max number of bytes, omitted reads to the end.

**Returns:** (`buffer?`, `string?`) File bytes as a Luau buffer, or nil plus an error message.

**Example:**

```lua
local buf, err = maki.fs.read_bytes("image.png")
if err then return end
local encoded = maki.base64.encode(buf)
local header = maki.fs.read_bytes("image.png", { len = 8 })
```

---

### `maki.fs.read_lines()` {#maki-fs-read_lines}

```lua
maki.fs.read_lines({path}, {opts?})
```

Read a page of lines from the file at {path}, holding only that page, and
count every line of the file in the same pass. With `max_line_bytes`, a
file of any size pages fine, even one that is a single huge line.
Lines come without their `\n` or `\r\n`, and a trailing newline does not
start a new line: `a\nb\n` has 2 lines.
Returns nil plus an error message if the page is over 64 MiB or not valid
UTF-8.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{path}` (`string`) Absolute or relative file path. `~/` is expanded to the home directory.
- `{opts?}` (`table?`) `offset` is the 1-based line to start at (default 1). `limit` is the max number of lines, `0` or omitted reads to the end. `max_line_bytes` cuts longer lines at a UTF-8 boundary and appends `...`, omitted or `0` keeps lines whole. `count = false` stops reading after the page and leaves `total` nil.

**Returns:** (`table?`, `string?`) `{ lines = string[], total = integer? }`, or nil plus an error message.

**Example:**

```lua
local page, err = maki.fs.read_lines("server.log", { offset = 101, limit = 100, max_line_bytes = 500 })
if err then return end
print(("lines 101-%d of %d"):format(100 + #page.lines, page.total))
```

---

### `maki.fs.metadata()` {#maki-fs-metadata}

```lua
maki.fs.metadata({path})
```

Get metadata for the file or directory at {path}.
Returns a table with `size` (integer), `is_file` (boolean), `is_dir` (boolean),
and `mtime` (number, fractional seconds since the Unix epoch; absent when the
filesystem does not report a modification time).
If {path} does not exist, returns nil with no error.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{path}` (`string`) Absolute or relative path.

**Returns:** (`table?`, `string?`) Metadata table, nil if missing, or nil plus an error message.

**Example:**

```lua
local meta = maki.fs.metadata("src/main.rs")
if meta and meta.is_file then
  print("size: " .. meta.size)
end
```

---

### `maki.fs.dirname()` {#maki-fs-dirname}

```lua
maki.fs.dirname({path})
```

Return the parent directory of {path}. Like `vim.fs.dirname`.

**Parameters:**

- `{path}` (`string`) File path.

**Returns:** (`string?`) Parent directory, or nil if {path} has no parent.

**Example:**

```lua
maki.fs.dirname("/home/user/init.lua") -- "/home/user"
```

---

### `maki.fs.basename()` {#maki-fs-basename}

```lua
maki.fs.basename({path})
```

Return the final component (the file name) of {path}. Like `vim.fs.basename`.

**Parameters:**

- `{path}` (`string`) File path.

**Returns:** (`string?`) File name, or nil for paths like `/`.

**Example:**

```lua
maki.fs.basename("/home/user/init.lua") -- "init.lua"
```

---

### `maki.fs.joinpath()` {#maki-fs-joinpath}

```lua
maki.fs.joinpath({...})
```

Join one or more path segments into a single path. Like `vim.fs.joinpath`.

**Parameters:**

- `{...}` (`string`) One or more path segments to join.

**Returns:** (`string`) The joined path.

**Example:**

```lua
maki.fs.joinpath("src", "api", "fs.rs") -- "src/api/fs.rs"
```

---

### `maki.fs.normalize()` {#maki-fs-normalize}

```lua
maki.fs.normalize({path})
```

Clean up `.` and `..` segments and make {path} absolute. Like `vim.fs.normalize`.
This is purely string-based and does not touch the filesystem.

**Parameters:**

- `{path}` (`string`) Path to normalize. `~/` is expanded.

**Returns:** (`string`) Normalized absolute path.

**Example:**

```lua
maki.fs.normalize("src/../src/api") -- "/home/user/project/src/api"
```

---

### `maki.fs.abspath()` {#maki-fs-abspath}

```lua
maki.fs.abspath({path})
```

Make {path} absolute by prepending the current working directory when needed.
Unlike `normalize`, this does not resolve `.` or `..` segments.

**Parameters:**

- `{path}` (`string`) Relative or absolute path. `~/` is expanded.

**Returns:** (`string`) Absolute path.

**Example:**

```lua
maki.fs.abspath("src/main.rs") -- "/home/user/project/src/main.rs"
```

---

### `maki.fs.parents()` {#maki-fs-parents}

```lua
maki.fs.parents({path})
```

Return all ancestor directories of {path}, from the immediate parent up to the root.
Handy for walking up a directory tree.

**Parameters:**

- `{path}` (`string`) File or directory path.

**Returns:** (`string[]`) Array of ancestor directory paths.

**Example:**

```lua
local dirs = maki.fs.parents("/home/user/project/src")
-- { "/home/user/project", "/home/user", "/home", "/" }
```

---

### `maki.fs.root()` {#maki-fs-root}

```lua
maki.fs.root({source}, {marker})
```

Walk upward from {source} looking for a directory that contains one of the
{marker} files or directories. Like `vim.fs.root`. Useful for finding the
project root.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{source}` (`string`) Starting file or directory path.
- `{marker}` (`string|string[]`) Marker filename(s) to look for, e.g. `".git"` or `{"package.json", ".git"}`.

**Returns:** (`string?`, `string?`) Root directory path, or nil when not found.

**Example:**

```lua
local root = maki.fs.root("src/main.rs", { ".git", "Cargo.toml" })
if root then print("project root: " .. root) end
```

---

### `maki.fs.relpath()` {#maki-fs-relpath}

```lua
maki.fs.relpath({base}, {target})
```

Compute a relative path from {base} to {target}.

**Parameters:**

- `{base}` (`string`) Base directory path.
- `{target}` (`string`) Target path.

**Returns:** (`string`) Relative path from {base} to {target}.

**Example:**

```lua
maki.fs.relpath("/home/user", "/home/user/project/src") -- "project/src"
```

---

### `maki.fs.ext()` {#maki-fs-ext}

```lua
maki.fs.ext({path})
```

Return the file extension of {path}, without the leading dot.

**Parameters:**

- `{path}` (`string`) File path.

**Returns:** (`string?`) Extension, or nil if the path has no extension.

**Example:**

```lua
maki.fs.ext("main.rs")   -- "rs"
maki.fs.ext("Makefile")  -- nil
```

---

### `maki.fs.dir()` {#maki-fs-dir}

```lua
maki.fs.dir({path}, {opts?})
```

List the contents of the directory at {path}.
Each entry is a two-element array `{name, type}` where type is one of
`"file"`, `"directory"`, `"link"`, or `"unknown"`. Follows symlinks.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{path}` (`string`) Directory path.
- `{opts?}` (`table?`) `depth` (integer, default 1): how many levels deep to recurse.

**Returns:** (`table?`, `string?`) Array of `{name, type}` entries, or nil plus an error message.

**Example:**

```lua
local entries, err = maki.fs.dir("src", { depth = 2 })
if err then return end
for _, e in ipairs(entries) do
  print(e[1], e[2]) -- "main.rs"  "file"
end
```

---

### `maki.fs.write()` {#maki-fs-write}

```lua
maki.fs.write({path}, {content})
```

Write {content} to the file at {path}, creating it if it does not exist
or overwriting it if it does.

Requires the `fs_write` [plugin permission](#plugin-permissions).

**Parameters:**

- `{path}` (`string`) Destination file path. `~/` is expanded.
- `{content}` (`string`) Text to write.

**Returns:** (`true?`, `string?`) `true` on success, or nil plus an error message.

**Example:**

```lua
local ok, err = maki.fs.write("out.txt", "hello world")
if err then print("write failed: " .. err) end
```

---

### `maki.fs.append()` {#maki-fs-append}

```lua
maki.fs.append({path}, {content})
```

Append {content} to the file at {path}, creating it (but not its parent
directory) if it does not exist.

Requires the `fs_write` [plugin permission](#plugin-permissions).

**Parameters:**

- `{path}` (`string`) Destination file path. `~/` is expanded.
- `{content}` (`string`) Text to append.

**Returns:** (`true?`, `string?`) `true` on success, or nil plus an error message.

**Example:**

```lua
local ok, err = maki.fs.append("out.log", "line\n")
if err then print("append failed: " .. err) end
```

---

### `maki.fs.atomic_write()` {#maki-fs-atomic_write}

```lua
maki.fs.atomic_write({path}, {content})
```

Atomically replace {path} with {content}. The parent directory must exist.
Readers observe either the old file or the complete new file.
Existing file permissions are preserved. On Unix, new files use mode 0600.

Requires the `fs_write` [plugin permission](#plugin-permissions).

**Parameters:**

- `{path}` (`string`) Destination file path. `~/` is expanded.
- `{content}` (`string`) Text to write.

**Returns:** (`true?`, `string?`) `true` on success, or nil plus an error message.

**Example:**

```lua
local ok, err = maki.fs.atomic_write("state.json", encoded)
if err then print("atomic write failed: " .. err) end
```

---

### `maki.fs.rm()` {#maki-fs-rm}

```lua
maki.fs.rm({path}, {opts?})
```

Delete the file, symlink, or directory at {path}.
Pass `recursive = true` to remove a non-empty directory tree (like `rm -r`).
Unlike `vim.fs.rm`, this also removes an empty directory without `recursive`.
Symlinks are removed themselves, never followed.

Requires the `fs_write` [plugin permission](#plugin-permissions).

**Parameters:**

- `{path}` (`string`) Path to the file or directory to remove.
- `{opts?}` (`table?`) `recursive` (boolean, default false): remove a directory and its contents recursively. `force` (boolean, default false): silently ignore a missing path.

**Returns:** (`true?`, `string?`) `true` on success, or nil plus an error message.

**Example:**

```lua
local ok, err = maki.fs.rm("temp.txt")
if err then print("rm failed: " .. err) end
maki.fs.rm("stale_dir", { recursive = true, force = true })
```

---

### `maki.fs.mkdir()` {#maki-fs-mkdir}

```lua
maki.fs.mkdir({path}, {opts?})
```

Create the directory at {path}. Set `parents = true` to create
intermediate directories, like `mkdir -p`.

Requires the `fs_write` [plugin permission](#plugin-permissions).

**Parameters:**

- `{path}` (`string`) Directory path to create.
- `{opts?}` (`table?`) `parents` (boolean, default false): create intermediate parent directories.

**Returns:** (`true?`, `string?`) `true` on success, or nil plus an error message.

**Example:**

```lua
maki.fs.mkdir("a/b/c", { parents = true })
```

---

### `maki.fs.glob()` {#maki-fs-glob}

```lua
maki.fs.glob({pattern}, {opts?})
```

Find files matching one or more glob patterns, walked fresh on every call.

Reads any path the plugin is allowed to read and keeps nothing afterwards.
`maki.fs.fuzzy_files` ranks a walk the host caches instead, which is
cheaper per keystroke but only covers the current working directory.

Respects `.gitignore` by default. Pass `sort = "mtime"` to get the most
recently modified files first.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{pattern}` (`string|string[]`) Glob pattern or array of patterns.
- `{opts?}` (`table?`) `path` (string): search root. `limit` (integer): max results. `gitignore` (boolean, default true): respect .gitignore. `sort` (string): `"mtime"` sorts newest first.

**Returns:** (`string[]?`, `string?`) Array of absolute file paths, or nil plus an error message.

**Example:**

```lua
local files, err = maki.fs.glob("**/*.lua", { path = "plugins", limit = 10 })
if err then return end
for _, f in ipairs(files) do print(f) end
```

---

### `maki.fs.grep()` {#maki-fs-grep}

```lua
maki.fs.grep({pattern}, {opts?})
```

Search file contents for a regex {pattern}. Returns structured matches
grouped by file, similar to ripgrep output.

Each result entry has a `path` and a list of `groups`. Each group contains
`lines`, where every line has `line_nr`, `text`, and `is_match`.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{pattern}` (`string`) Regular expression to search for.
- `{opts?}` (`table?`) `path` (string): search root. `include` (string): file glob filter (e.g. `"*.rs"`). `context_before` / `context_after` (integer): context lines around matches. `limit` (integer): max match groups. `max_line_bytes` (integer): truncate lines longer than this (0 means unlimited).

**Returns:** (`table?`, `string?`) Array of `{path, groups}` tables, or nil plus an error message.

**Example:**

```lua
local hits, err = maki.fs.grep("TODO", { path = "src", include = "*.rs", limit = 5 })
if err then return end
for _, file in ipairs(hits) do
  for _, g in ipairs(file.groups) do
    for _, line in ipairs(g.lines) do
      if line.is_match then print(file.path .. ":" .. line.line_nr) end
    end
  end
end
```

---

### `maki.fs.fuzzy_files()` {#maki-fs-fuzzy_files}

```lua
maki.fs.fuzzy_files({opts?})
```

Rank the files and directories under {opts.path} against {opts.query} and
return the best ones, best first, with the state of the walk behind them.

The host walks each root once and shares that walk with the built-in file
picker (`Ctrl+S`), so a call ranks an existing list instead of walking the
tree, and both rank alike. Use `maki.fs.glob` for patterns, a path outside
the cwd, or a tree read fresh right now. Only `limit` items ever cross into
Lua whatever the size of the repo, paths come back relative to the root
with a trailing separator on the directories, and `.gitignore` and `.git`
are respected. `highlights` cost a second matcher pass, so ask for them
only to draw matches.

`complete` is false while a walk is filling the list, so an empty `items`
means "not found yet" and asking again is worth it. maki walks a bounded
number of trees at once, so a first call can also answer before the walk it
asked for has started. `crashed` and `truncated` are the two ways a
complete list is still not the whole tree: a walker that died partway
through it, and a tree bigger than the host's ceiling. Listen for
`"FileIndexReady"` with `maki.api.create_autocmd` to be told when a walk
lands rather than polling for it.

The tools that write, move and delete files mark the tree they touched, so
a call a moment after an edit re-walks and a file the agent just wrote is
findable. A file a `bash` command creates or deletes is not: a shell
command cannot say what it touched, and maki does not try to guess. That
leaves the staleness window as the only guarantee, and it is this: a walk
is redone the first time anything asks for the index more than twenty
seconds after the last one landed, so a path may be missing from the list,
or offered after it is gone, for up to twenty seconds.

A second call from the same plugin cancels the one in flight, which answers
with nil plus an error. While the user types that is expected, so treat the
error as a stale answer. A plugin reading more than four roots keeps the
four it asked for most recently: the walk behind an older one is let go and
asking for it again walks it again.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{opts?}` (`table?`) Options:
  - `query` (`string`) what the user typed. Empty returns the first `limit` paths in walk order.
  - `limit` (`integer`) how many items to return, at most 500. Default 20.
  - `path` (`string`) the root to search, the cwd or below it. Default is the current working directory.
  - `highlights` (`boolean`) also return where the query matched each path. Default false.

**Returns:** (`table?`, `string?`) `{ root = string, complete = boolean, crashed = boolean, truncated = boolean, items = { { path = string, highlights = integer[][]? } } }`, or nil plus an error message. `root` is the resolved absolute directory the paths are relative to, spelled the way `"FileIndexReady"` spells it, so an event can be matched to the call that caused it. It is absent for a directory with no UTF-8 spelling, as it is on the event, so a plugin that gets no `root` treats every event as a reason to ask again rather than matching the wrong tree. `items` is a 1-based array, best first. `highlights` is only present when asked for, and holds `{ from, to }` byte ranges of `path`, ascending, 1-based and inclusive, so `path:sub(from, to)` is the matched text.

**Example:**

```lua
local res, err = maki.fs.fuzzy_files({ query = "src/mai", limit = 20, highlights = true })
if err then return end -- a newer call took over, this answer is stale
for _, item in ipairs(res.items) do
  local r = item.highlights[1]
  print(item.path, r and item.path:sub(r[1], r[2]))
end
if not res.complete then print("still scanning, ask again") end
```


## maki.image {#maki-image}

Small building blocks for working with images: probe metadata, decode
pixels, resize, and encode back to bytes. Plugins compose these freely.

Decoding is guarded against pixel-bomb attacks (50 MP limit).

```lua
local img = maki.image.decode(raw_bytes)
local small = img:resize(1024, 768)
local png = small:encode("png")
```

---

### `maki.image.probe()` {#maki-image-probe}

```lua
maki.image.probe({data})
```

Read image metadata (format, dimensions) from raw bytes without fully
decoding the pixels. Much faster than `decode` when you only need to
check the size or format.

Returns a table with `format` (string), `width` (integer), `height`
(integer), or `(nil, err)` if the bytes are not a recognized image.

**Parameters:**

- `{data}` (`string|buffer`) Raw image bytes.

**Returns:** (`table?`, `string?`) Info table, or `(nil, err)` on failure.

**Example:**

```lua
local info, err = maki.image.probe(raw_bytes)
if err then error(err) end
print(info.format, info.width, info.height)
```

---

### `maki.image.decode()` {#maki-image-decode}

```lua
maki.image.decode({data})
```

Decode raw image bytes into an Image handle you can resize and re-encode.
The whole image is decoded up front, so corrupt input fails here rather
than later. Images larger than 50 megapixels are rejected to prevent
memory bombs. Formats other than png, jpeg, gif, and webp (like bmp) are
converted to png, so the image can go into a message.

**Parameters:**

- `{data}` (`string|buffer`) Raw image bytes.

**Returns:** ([`maki.image.Image?`](#maki-image-Image), `string?`) Decoded image, or `(nil, err)` on failure.

**Example:**

```lua
local img, err = maki.image.decode(raw_bytes)
if err then error(err) end
print(img:width() .. "x" .. img:height())
```


## maki.image.Image {#maki-image-Image}

An image you can inspect, resize, and re-encode.

Get one from `maki.image.decode()`. It always holds encoded bytes as
png, jpeg, gif, or webp, and decodes the pixels on first use. As a
message image source or through `maki.json.encode` it becomes
`{ type = "base64", media_type, data }`. The data lives in memory
until the handle is garbage collected.

---

### `Image:width()` {#Image-width}

```lua
Image:width()
```

Get the width of the image in pixels. Reads only the header when the
pixels are not decoded yet.

**Returns:** (`integer`) Width in pixels.

---

### `Image:height()` {#Image-height}

```lua
Image:height()
```

Get the height of the image in pixels. Reads only the header when the
pixels are not decoded yet.

**Returns:** (`integer`) Height in pixels.

---

### `Image:resize()` {#Image-resize}

```lua
Image:resize({max_w}, {max_h})
```

Shrink the image to fit inside {max_w} x {max_h}, keeping the aspect
ratio. If the image already fits, it is returned as-is. Never upscales.
A resized image is JPEG if the source was JPEG and PNG otherwise, which
decides its `media_type` when placed into a message.

**Parameters:**

- `{max_w}` (`integer`) Maximum width in pixels. Must be positive.
- `{max_h}` (`integer`) Maximum height in pixels. Must be positive.

**Returns:** ([`maki.image.Image`](#maki-image-Image)) A new image handle (or the same one if no resize was needed).

**Example:**

```lua
local img = maki.image.decode(raw_bytes)
local small = img:resize(800, 600)
local encoded = small:encode("jpeg")
```

---

### `Image:encode()` {#Image-encode}

```lua
Image:encode({format})
```

Encode the image into raw bytes in the given format. Use this to prepare
images for sending over the network or writing to disk. Bytes that came
from outside are always re-encoded, so this can shrink an oversized file.
An image from `resize` asked for its own format comes back as is.

**Parameters:**

- `{format}` (`string`) Output format: `"png"`, `"jpeg"`, or `"jpg"`.

**Returns:** (`string`) Encoded image bytes.

**Example:**

```lua
local bytes = img:encode("png")
-- bytes is a Lua string containing the raw PNG data
```


## maki.interpreter {#maki-interpreter}

Run Python code in a memory-safe, time-limited sandbox.

The sandbox uses the monty interpreter. Python code can call back into
Lua-defined tools, and stdout is streamed line by line.

```lua
local r, err = maki.interpreter.run("print('hello')", {
  timeout = 10,
  max_memory_mb = 128,
  on_output = function(line) print(line) end,
})
```

---

### `maki.interpreter.run()` {#maki-interpreter-run}

```lua
maki.interpreter.run({code}, {opts})
```

Run Python code in a sandboxed interpreter with memory and time limits.
Stdout lines are streamed to your {on_output} callback as they are produced.
If the Python code calls tools, those calls are dispatched to the Lua
functions you provide in {opts}.tools.

The result table has optional fields: `stdout` (string, trimmed combined
output) and `output` (string, the final expression value). On error, the
table is empty and the second return value is the error message.

Requires the `run` [plugin permission](#plugin-permissions).

**Parameters:**

- `{code}` (`string`) Python source code to execute.
- `{opts}` (`table`) Required fields:
  - `timeout` (`integer`) execution time limit in seconds.
  - `max_memory_mb` (`integer`) memory limit in megabytes.
  - `on_output` (`function`) called with each stdout line (string) as it is
    produced. Must not yield.

  Optional fields:

  - `preamble` (`string?`) Python source (imports, helpers) compiled ahead of
    {code}. Tracebacks are rebased so line 1 is {code} line 1.
  - `tools` (`table?`) map of `name -> function` for tools the sandbox may call.
    Each function receives the tool input table and must return `(string)` or
    `(nil, err)`. Tool calls are batched and dispatched concurrently.
  - `files` (`table?`) callbacks that serve `open()`, `os` and `pathlib`.
    Without it, any OS call ends the run. A missing callback refuses that
    access, and `(nil, err)` refuses one call. Binary I/O, removing and
    renaming always raise `RuntimeError`. `os.getenv` sees only non-secret
    variables like `HOME`. Sleeps and metadata queries, `inspect` included,
    share a budget of `timeout` seconds, and going over ends the run.
    - `read(path)` returns `(content)`. A refusal raises `OSError`.
    - `write(path, content, append)` returns `(string)` or `(true)`. Writes
    are buffered per file, and flushed before a read of that file, before
    any tool call or metadata query, and at the end of the run. A
    cancelled run drops them. A failed flush ends the run, unless a read
    triggered it, then the read raises `OSError`.
    - `inspect(path)` gets the absolute path of each metadata query
    (`os.stat`, `os.walk`, `Path.glob`) and returns `(true)` to allow it.
    A walk or glob covers everything under {path}. A refusal raises
    `RuntimeError`. Needs the `fs_read` permission.
    - `mkdir(path, parents)` returns `(true)` once the directory exists.
    It only gets missing paths that passed `inspect`. A refusal raises
    `OSError`.

**Returns:** (`table`, `string?`) Result table, plus an error string on failure.

**Example:**

```lua
local result, err = maki.interpreter.run("print(2 + 2)", {
  timeout = 30,
  max_memory_mb = 256,
  on_output = function(line) print("py: " .. line) end,
})
if err then error(err) end
if result.stdout then print(result.stdout) end
```


## maki.json {#maki-json}

JSON encoding, decoding, and schema validation. Encode Lua
tables to JSON strings, decode JSON back into tables, and
optionally validate data against a JSON Schema.

```lua
local s = maki.json.encode({ ok = true })
local t = maki.json.decode(s)
```

---

### `maki.json.encode()` {#maki-json-encode}

```lua
maki.json.encode({value})
```

Turn a Lua value into a JSON string. Tables, strings, numbers,
booleans, nil, and `maki.NIL` (which becomes null) all work. A
`maki.image.Image` becomes `{ type = "base64", media_type, data }`.
Functions, threads, other userdata, and tables that contain themselves
cannot be encoded.

A table is an array when it came from a JSON array (`maki.json.decode`,
`maki.deepcopy` of one), or when all its keys are positive integers and
its largest key is at most 10 or at most twice the number of keys, with
null in the gaps like `vim.json.encode`. Any other table is an object,
with number and boolean keys written as strings.

**Parameters:**

- `{value}` (`any`) Lua value to encode.

**Returns:** (`string?`, `string?`) JSON string, or nil plus an error.

**Example:**

```lua
local s, err = maki.json.encode({ name = "maki", version = 1 })
print(s) -- {"name":"maki","version":1}
```

---

### `maki.json.decode()` {#maki-json-decode}

```lua
maki.json.decode({str}, {opts?})
```

Parse a JSON string into a Lua value. Objects become tables and
arrays become 1-indexed sequences. Every JSON null becomes `maki.NIL`, so
a key holding null is still there and encoding the result gives the same
JSON back. `maki.NIL` is a special value that stands for JSON null. It is
truthy, so test for null with `x == maki.NIL`.

Pass `opts.luanil` to get plain nil instead, like `vim.json.decode`.
`luanil.object` does so for nulls inside objects and `luanil.array` for
nulls inside arrays. A null at the top level is always `maki.NIL`.

**Parameters:**

- `{str}` (`string`) JSON string to decode.
- `{opts?}` (`table?`) Optional. `luanil` (table with boolean `object` and `array`) turns those nulls into nil.

**Returns:** (`any?`, `string?`) Decoded value, or nil plus an error.

**Example:**

```lua
local t, err = maki.json.decode('{"x": 42, "y": null}')
print(t.x) -- 42
print(t.y == maki.NIL) -- true

local u = maki.json.decode('{"y": null}', { luanil = { object = true } })
print(u.y) -- nil
```

---

### `maki.json.schema_validator()` {#maki-json-schema_validator}

```lua
maki.json.schema_validator({schema})
```

Compile a JSON Schema into a reusable validator object. Supports
draft-07, 2019-09, and 2020-12. Schema errors show up right away so
you catch mistakes before doing any real work.

**Parameters:**

- `{schema}` (`table`) JSON Schema as a Lua table.

**Returns:** ([`maki.json.SchemaValidator?`](#maki-json-SchemaValidator), `string?`) Validator, or nil plus an error.

**Example:**

```lua
local v, err = maki.json.schema_validator({
  type = "object",
  properties = { name = { type = "string" } },
  required = { "name" },
})
local errs = v:validate({ name = "maki" })
assert(errs == nil)
```


## maki.json.SchemaValidator {#maki-json-SchemaValidator}

A compiled JSON Schema validator. Create one with `maki.json.schema_validator()` and reuse it to validate many values without recompiling the schema each time.

---

### `SchemaValidator:validate()` {#SchemaValidator-validate}

```lua
SchemaValidator:validate({value})
```

Check {value} against the compiled schema. Returns nil when the value is valid. When validation fails, returns a list of human-readable error strings.

**Parameters:**

- `{value}` (`any`) The Lua value to validate.

**Returns:** (`table?`) Array of error strings, or nil if valid.

**Example:**

```lua
local errs = validator:validate({ name = 123 })
if errs then
for _, msg in ipairs(errs) do print(msg) end
end
```


## maki.keymap {#maki-keymap}

Key mappings, modeled after `vim.keymap`.

```lua
maki.keymap.set({ "n", "i" }, "<C-t>", function()
  print("hello")
end, { desc = "Say hello" })
```

## Modes

As in Neovim, maki is in one mode at a time, and a binding fires only
in the modes it was set for.

| Mode | When |
| --- | --- |
| `"i"` (insert, the default) | The chat input has the keyboard, and keys type into it. |
| `"n"` (normal) | Set by `maki.ui.set_mode("n")`, and while a picker, modal, prompt or focused window has the keyboard or a finished subagent's chat is shown. |

The mode a plugin set comes back when the chat input gets the keyboard
again. In normal mode, keys never type or delete in the chat input.
Moving the cursor, history, pasting and `<CR>` to send still work.

Bind a shortcut in `{ "n", "i" }` to make it work everywhere, as the
built-in plugins do. Read the mode with `maki.fn.mode()`, and watch it
with the `"ModeChanged"` autocmd.

```lua
-- A tiny vim mode: Esc leaves insert mode, i goes back.
maki.keymap.set("i", "<Esc>", function() maki.ui.set_mode("n") end)
maki.keymap.set("n", "i", function() maki.ui.set_mode("i") end)

-- dd deletes the cursor's line. A lone d expires after 500 ms.
local pending
maki.keymap.set("n", "d", function(ev)
  if not pending then
    pending = maki.defer_fn(function() pending = nil end, 500)
    return
  end
  pending:stop()
  pending = nil
  local st = ev.input
  local head = st.text:sub(1, st.cursor):match("^.*\n")
  maki.ui.input_edit({
    start = head and #head or 0,
    stop = st.text:find("\n", st.cursor + 1, true) or #st.text,
    text = "", version = st.version, session_id = st.session_id,
  })
end)
```

With this `<Esc>` binding, the built-in `Esc Esc` (rewind) works only
from normal mode.

Older versions fired `"n"` bindings while you typed. See
[Upgrading old keymaps](/docs/keybindings/#upgrading-old-keymaps).

## Key resolution

Each key runs one handler: the first match in this list, for the
current mode.

1. `<C-c>` and `<C-z>`, which no binding can take. `<C-z>` suspends
   here, and `<C-c>` still goes to step 2 first.
2. Maki's modals and prompts, then the focused window, then Maki's
   pickers. They take every key, except the plan form and the `/`
   command list, which pass on the keys they do not use. The focused
   window runs its buffer's bindings, then its `win:recv`.
3. Windows on screen, front first: each one's buffer bindings, then
   the keys it claimed with the `keys` option of `maki.ui.open_win`.
4. `<Esc>` while the agent runs, in either mode. Maki takes it, and a
   second one in a row cancels the turn.
5. Global bindings, the last `set` first.
6. Maki's built-in keys and the chat input.

`ev.fallback()` jumps to step 6, or to `win:recv` for a binding on the
focused window's buffer. `<Paste>` follows the same list.

To share one key between plugins, see
[Composing keys](/docs/keybindings/#composing-keys).

## Key notation

`set`, `del`, the `keys` option of `maki.ui.open_win` and `win:recv`
key events all use one notation. `normalize` converts any accepted
spelling to the canonical one.

```lua
if ev.type == "key" and ev.key == "<CR>" then submit() end
```

A single character stands for itself: `a`, `A`, `7`, `?`. Other keys
go in angle brackets, after any modifiers.

| Key | Notation | Also accepted |
| --- | --- | --- |
| Enter | `<CR>` | `<Enter>`, `<Return>` |
| Escape | `<Esc>` | `<Escape>` |
| Backspace | `<BS>` | `<Backspace>` |
| Delete | `<Del>` | `<Delete>` |
| Tab | `<Tab>` | |
| Shift+Tab | `<S-Tab>` | |
| Space | `<Space>` | |
| Arrows | `<Up>`, `<Down>`, `<Left>`, `<Right>` | |
| Navigation | `<Home>`, `<End>`, `<PageUp>`, `<PageDown>`, `<Insert>` | |
| Function keys | `<F1>` through `<F24>` | |
| Bracketed paste | `<Paste>` | |

Modifiers are `C-` (control), `M-` (alt) and `S-` (shift), in that
order: `<C-M-x>`. `Ctrl-`, `Alt-`, `A-` and `Shift-` are accepted as
input.

Terminals report some keys differently, so maki picks one form:

- Control plus a letter is lowercase: `<C-N>` is `<C-n>`, as in Vim.
- Shift plus a letter is the uppercase letter: `<S-a>` is `A`.
- Without control or alt, shift is part of the char typed, so the key
  is that char: Shift+1 on a US layout is `!`, and `<S-!>` is `!`.
  `<S-Space>` is `<Space>`. With alt the prefix stays: `<M-S-1>`.
- Shift+Tab is always `<S-Tab>`, with or without the kitty keyboard
  protocol.

Key strings in a plugin and every module it `require`s are checked at
load. Each invalid one is logged with its file and line, and the status
bar shows a summary, so a typo shows up at startup.

Upgrading from older versions: `win:recv` used to deliver `"enter"`,
`"esc"`, `"ctrl+n"` and `"shift+tab"`. These now arrive as `<CR>`,
`<Esc>`, `<C-n>` and `<S-Tab>`, and the load check flags the old
spellings.

---

### `maki.keymap.set()` {#maki-keymap-set}

```lua
maki.keymap.set({mode}, {lhs}, {rhs}, {opts?})
```

Bind a key to a Lua function, like `vim.keymap.set`.

The binding fires only in the [modes](/docs/lua-api/#modes) you list. A
key bound in `"i"` no longer types, so bind keys like `<Tab>` or `<C-x>`
there, rarely a letter. A binding on a key that types text, like `a`,
`<Space>` or `<Paste>`, needs `session_read` in every mode, because it
sees what the user types. A plugin may switch the mode itself, so a
letter bound in `"n"` could hear every key the user meant to type. That
goes for a buffer binding too, since its window can sit on screen while
the user types into the chat input. Keys
with `C-` or `M-`, function keys and keys like `<CR>`, `<Esc>`, `<Tab>`,
`<BS>` or the arrows need nothing.
[Key resolution](/docs/lua-api/#key-resolution) shows which binding gets
a key.

Bindings belong to the plugin that set them and stack per mode. The last
`set` wins and logs a warning naming the plugin it shadows. When that
plugin calls `del` or unloads, the shadowed binding comes back. Setting a
key you already hold replaces your binding.

A binding with `opts.buffer` comes before any global binding and goes
away with the buffer. Use it for the keys of a popup. A handler that
refers to the buffer keeps it alive, so `del` it when the popup closes.

The handler consumes the key, even when it throws (the error is logged).
Call `ev.fallback()` to let maki handle the key after all. A handler that
changes the text of the input needs `input`, `agent` and `session_write`,
see [chat input](/docs/lua-api/#chat-input).

While a handler runs, the keys, pastes and clicks after it wait, so its
edits and mode switches are in place before the next key lands. Keys
wait one second at most, and stop waiting when another window takes the
keyboard, like a picker the handler opened. If a modal or a prompt opens
meanwhile, like an alert, a permission prompt or a question, the waiting
keys are dropped so they cannot answer it, and the status bar says so.

Every key waits its turn, also `<Esc>` while the agent runs, except
`<C-c>` and `<C-z>`. A `<C-c>` drops the pending `ev.fallback()`. If keys
were waiting, it drops them too and the status bar says so, and only the
next `<C-c>` works as usual. Otherwise it works as usual right away.

A key never sends text that changed after it was pressed, because the
user never saw what it would send. If the input changed or the keyboard
moved while a `<CR>` waited, maki drops that send and asks the user to
press Enter again. The same goes for `ev.fallback()` on `<CR>` after any
change since the press, your handler's own edits included. To send what
your handler wrote, use
[`maki.ui.input_submit`](/docs/lua-api/#maki-ui-input_submit).

Throws on an invalid mode or key, on `<C-c>` and `<C-z>`, which stay
reserved for quit and suspend, on a taken key with `unique`, and with
`permission denied` on a typing key in `"i"` without `session_read`.

**Parameters:**

- `{mode}` (`string|string[]`) `"n"`, `"i"`, or a list of them.
- `{lhs}` (`string`) Key in Vim notation, e.g. `"<C-t>"`, `"a"`, or `"<Paste>"` for bracketed paste.
- `{rhs}` (`function`) Called with an event table `ev`. The return value is ignored. `ev` has:
  - `key` (`string`) the key, in canonical notation.
  - `input` (`table?`) the chat input at the press, as `maki.ui.input` returns it. Pass its `version` and `session_id` to `maki.ui.input_edit`. Only with `session_read`, else nil.
  - `text` (`string?`) `<Paste>` only: the pasted text, with `\n` line endings. maki does not insert it. It is cleaned the way `maki.ui.input_edit` cleans text, so `#ev.text` is what inserting it adds. Only with `session_read`, else nil.
  - `fallback` (`function`) runs maki's own handling of the key, as if no plugin had bound it (see [Key resolution](/docs/lua-api/#key-resolution)). Call it once, before the handler returns.
    - It lands after the handler's earlier `maki.ui` calls and before the waiting keys.
    - It returns `true` once the key is queued, or `nil` and an error.
    - The key is dropped if the keyboard, the mode, the cursor or the selection moved since the press, your handler's own changes included.
- `{opts?}` (`table?`) Options:
  - `desc` (`string`) shown next to the key in `/help`.
  - `unique` (`boolean`) throw, naming the owner, when the key is already
    bound in one of the modes. Default false.
  - `buffer` (`Buf`) bind the key only while a window showing this buffer is
    on screen.

**Example:**

```lua
-- Tab in an empty input starts a review prompt, else it switches plan/build:
maki.keymap.set("i", "<Tab>", function(ev)
  local st = ev.input
  if st.text ~= "" then
    return ev.fallback()
  end
  maki.ui.input_edit({
    start = 0, stop = 0, text = "Review the staged changes: ",
    version = st.version, session_id = st.session_id,
  })
end)

-- Keys of a popup, live only while it is on screen:
local buf = maki.ui.buf()
local win = maki.ui.open_win(buf, { focus = false, width = 20, height = 3 })
maki.keymap.set("i", "<Esc>", function()
  maki.keymap.del("i", "<Esc>", { buffer = buf })
  win:close()
end, { buffer = buf })
```

---

### `maki.keymap.del()` {#maki-keymap-del}

```lua
maki.keymap.del({mode}, {lhs}, {opts?})
```

Remove your plugin's binding for {lhs} in {mode}, like `vim.keymap.del`.
The key goes back to the binding yours shadowed, or to maki's default.

Only your own bindings go. If another plugin holds {lhs}, nothing
changes and a warning names it. Only the modes you list are removed: a
binding set in `{ "n", "i" }` and removed in `"n"` stays in `"i"`.
Throws on an invalid mode or key.

**Parameters:**

- `{mode}` (`string|string[]`) `"n"`, `"i"`, or a list of them.
- `{lhs}` (`string`) Key to unmap, in Vim notation.
- `{opts?}` (`table?`) Options:
  - `buffer` (`Buf`) remove the binding set for this buffer instead of the
    global one.

**Example:**

```lua
maki.keymap.del({ "n", "i" }, "<C-t>")
```

---

### `maki.keymap.normalize()` {#maki-keymap-normalize}

```lua
maki.keymap.normalize({lhs})
```

Canonical spelling of {lhs}. Accepts every spelling `set` accepts and
returns the string a `key` event carries.

**Parameters:**

- `{lhs}` (`string`) Key in any accepted notation.

**Returns:** (`string|nil`, `string|nil`) Canonical notation, or nil and an error.

**Example:**

```lua
local canon = maki.keymap.normalize("<Enter>")  -- "<CR>"
```


## maki.log {#maki-log}

Structured logging for plugins.

Each call emits a tracing event tagged with the calling plugin's name.
Messages show up in maki's log output, which you can view with `maki --log`.

```lua
maki.log.info("ready")
maki.log.warn("something looks off")
```

---

### `maki.log.debug()` {#maki-log-debug}

```lua
maki.log.debug({msg})
```

Emit a DEBUG-level log message. Useful for development and troubleshooting.
The message is tagged with the plugin name automatically.

**Parameters:**

- `{msg}` (`string`) Message to log.

**Example:**

```lua
maki.log.debug("loaded " .. #items .. " items")
```

---

### `maki.log.info()` {#maki-log-info}

```lua
maki.log.info({msg})
```

Emit an INFO-level log message. Good for normal operational events.

**Parameters:**

- `{msg}` (`string`) Message to log.

**Example:**

```lua
maki.log.info("plugin initialized")
```

---

### `maki.log.warn()` {#maki-log-warn}

```lua
maki.log.warn({msg})
```

Emit a WARN-level log message. Use for recoverable problems.

**Parameters:**

- `{msg}` (`string`) Message to log.

**Example:**

```lua
maki.log.warn("config file missing, using defaults")
```

---

### `maki.log.error()` {#maki-log-error}

```lua
maki.log.error({msg})
```

Emit an ERROR-level log message. Use for failures that need attention.

**Parameters:**

- `{msg}` (`string`) Message to log.

**Example:**

```lua
maki.log.error("failed to connect to API")
```


## maki.model {#maki-model}

The model behind the focused session. Good for a keybind that flips
between your two go-to models, or lifts thinking for one hard question.
Without an interactive UI every function returns
`nil, "no interactive UI attached"`.

---

### `maki.model.get()` {#maki-model-get}

```lua
maki.model.get()
```

Reads the focused session's model, thinking level, and fast mode.
`thinking` comes back in the spelling `set` accepts, so a table from here
can go straight back in.

`thinking_options` lists named thinking choices, cheapest first, as
`{name, tokens?}` per row. Request paths that use named `thinking_fields`
expose those levels without token counts. Toggle-only fields expose
`adaptive` and, when allowed, `off`. Otherwise the generic budget ladder applies.
`tokens` is absent on `off` and `adaptive` too. The list is empty exactly
when `supports_thinking` is false.

`thinking` keeps the session's intent and delegation ceiling.
`thinking_shown` names the effective row to highlight, including snapped
effort levels and budgets sent as native levels or toggles. With declared
fields, a budget stays numeric only where the adapter forwards the count.
On the generic ladder, a model whose API has no adaptive mode leaves
`adaptive` out and shows its fixed level.
Declared effort fields keep `adaptive` separate because their fragments can
differ from the provider fallback.

**Returns:** (`table|nil`, `string|nil`) `{spec, id, provider, thinking,
  thinking_shown, thinking_options, fast, supports_thinking,
  supports_fast}`, or nil and an error.

**Example:**

```lua
local m = maki.model.get()
if m.spec ~= "anthropic/claude-opus-4-6" then ... end
for _, option in ipairs(m.thinking_options) do
  print(option.name, option.tokens)
end
```

---

### `maki.model.available()` {#maki-model-available}

```lua
maki.model.available()
```

Lists the model specs you can switch to: what the providers you are logged
into offer, minus what your model policy blocks. The list fills in the
background at startup, so right after launch it can still be empty.

**Returns:** (`table|nil`, `string|nil`) Array of `"provider/id"` specs, or nil and an error.

**Example:**

```lua
local specs = maki.model.available()
```

---

### `maki.model.set()` {#maki-model-set}

```lua
maki.model.set({opts})
```

Switches the focused session's model, thinking level, or fast mode. Fields
you leave out stay as they are, so this doubles as a thinking-only switch.
Answers with the new state, in the same shape `get` returns.

Requires the `session_settings` [plugin permission](#plugin-permissions).

**Parameters:**

- `{opts}` (`string|table`) A model spec, or a table with any of:
  - `spec` (`string`) `"provider/id"`, as listed by `available()`;
  - `thinking` (`string|number`) `"off"`, `"adaptive"`, an effort level

  (`"minimal"` to `"max"`), a token budget, or `""` to toggle it on and off;

  - `fast` (`boolean`) Anthropic fast mode.

**Returns:** (`table|nil`, `string|nil`) The new state, or nil and an error.

**Example:**

```lua
maki.model.set("anthropic/claude-opus-4-6")
maki.model.set({ spec = "zai/glm-5", thinking = "high" })
maki.keymap.set({ "n", "i" }, "<M-t>", function() maki.model.set({ thinking = "" }) end)
```

---

### `maki.model.tier()` {#maki-model-tier}

```lua
maki.model.tier({name}, {provider?})
```

The model maki uses for a tier, the same one a subagent asking for that
tier gets: user's pick from the `/model` picker for that tier (whatever
provider it names), else the curated default within `provider`, else any
provider. `assigned` tells a pick apart from a default. Pass `provider` to
prefer its models, the way subagents prefer the session's provider.

**Parameters:**

- `{name}` (`string`) `"weak"`, `"medium"`, `"strong"`, or `"compaction"`.
- `{provider?}` (`string|nil`) Provider slug to resolve within first.

**Returns:** (`table|nil`, `string|nil`) The model in the shape `info` returns, plus
  `assigned` (boolean), true when you picked it for this tier. nil and nil
  when no model fits the tier, nil and an error when the spec no longer
  resolves.

**Example:**

```lua
local m = maki.model.tier("weak", maki.model.get().provider)
if m then maki.model.set(m.spec) end
```

---

### `maki.model.info()` {#maki-model-info}

```lua
maki.model.info({spec})
```

Resolve a model spec to everything maki knows about it: identity, tier,
context window, and the price table the session would be billed by --
including rates resolved from provider config or the bundled catalog
(e.g. subsidised custom providers), which the provider's own /v1/models
endpoint may never report. Purely local -- no UI round-trip, no network
-- so it also works from slash commands and headless embeddings.

**Parameters:**

- `{spec}` (`string`) `"provider/id"`, as listed by `available()`.

**Returns:** (`table|nil`, `string|nil`) `{spec, id, provider, provider_display,
  tier, subsidised_by?, context_window, max_output_tokens?, free?,
  pricing?}`, or nil and an error.

  `free` has three states: `true` when the model is known to cost nothing,
  `false` when it is metered, and nil when no source ever quoted a rate.
  Check `~= nil` before trusting it.

  `subsidised_by` names the subscription prepaying this provider (billed
  cost is $0, the rates are the list-price reference). It sits at the top
  level because it holds whether or not rates resolved.

  `pricing` is present only when rates are known:
  `{input, output, cache_write, cache_read}` in USD per million tokens,
  plus optional `fast = {input, output}`.

**Example:**

```lua
local m, err = maki.model.info("anthropic/claude-opus-4-6")
if m and m.subsidised_by then print(m.subsidised_by, m.pricing.input) end
```


## maki.net {#maki-net}

HTTP and plain TCP for plugins.

`request` traffic goes over HTTPS (plain HTTP is upgraded). Private
and metadata IP addresses are blocked to prevent SSRF, including
after a redirect. Hosts listed in the `net.allowed_private_hosts` config
option are exempt, and so is a provider plugin's own origin (see
`maki.net.request`). Failed requests (5xx) are retried automatically.

Requests reuse a pool of clients, so calls to the same host share one
keep-alive connection rather than pay a fresh handshake each time.

`connect` follows the same rules, but only reaches hosts the plugin
lists in `net_hosts`.

```lua
local res, err = maki.net.request("https://example.com")
if res then print(res.body) end
```

---

### `maki.net.request()` {#maki-net-request}

```lua
maki.net.request({url}, {opts?})
```

Make an HTTP request and return the response body. Plain `http://`
URLs are automatically upgraded to `https://`. Requests to private
or metadata IP addresses are blocked for safety, unless the host is
listed in `net.allowed_private_hosts`.

A request to the origin of a provider this plugin registered is sent
like the provider's chat requests: no address check, no https upgrade,
maki's user agent, and connect and stall timeouts instead of a total
one. This holds only for an origin the user set (`<SLUG>_BASE_URL`,
`providers.toml`) or a built-in provider's default.

{opts} fields:
- `method` (`string`) HTTP verb (default `"GET"`).
- `headers` (`table`) Header name/value pairs.
- `body` (`string`) Request body.
- `timeout` (`integer`) Total timeout in seconds, max 120 (default 30,
  none on a provider's origin).
- `max_bytes` (`integer`) Max response size in bytes (default 5 MB).
- `retry` (`integer`) Retries on 5xx errors (default 3).
- `line_match` (`string`) Regex. Keep only the response lines it
  matches. Filtering happens after the body is read, so `max_bytes`
  still caps the transfer.

The response table has `body` (string), `status` (integer),
`content_type` (string) and `headers` (table). `headers` holds the final
response's headers under lowercase names, as in
`res.headers["retry-after"]`. Repeated headers are joined with `, `,
which breaks `set-cookie`. A failed response can go straight to
`maki.provider.http_error`.

Requires the `net` [plugin permission](#plugin-permissions).

**Parameters:**

- `{url}` (`string`) URL starting with `http://` or `https://`.
- `{opts?}` (`table?`) Request options (see above).

**Returns:** (`table?`, `string?`) Response table, or nil plus an error string.

**Example:**

```lua
local res, err = maki.net.request("https://httpbin.org/get")
if err then
  print("failed: " .. err)
else
  print(res.status, res.body)
end
```

---

### `maki.net.connect()` {#maki-net-connect}

```lua
maki.net.connect({host}, {port}, {opts?})
```

Open a plain TCP connection to {host}:{port}, such as a dashboard or a
language server running on your machine. There is no TLS.

The plugin must list the host in `net_hosts`, best with its port
(`"127.0.0.1:7777"`): unlike `request`, `net = true` alone reaches
nothing. Private and loopback addresses are blocked like in `request`,
unless `net.allowed_private_hosts` allows them.

`read` and `write` yield, so a connection that stays open belongs in a
`maki.async.spawn` task. It closes on `conn:close()`, when the handle is
garbage collected, and when the plugin unloads.

{opts} fields:
- `timeout` (`integer`) Connect timeout in seconds, max 60 (default 10).

Requires the `net` [plugin permission](#plugin-permissions).

**Parameters:**

- `{host}` (`string`) Host name or IP address.
- `{port}` (`integer`) Port to connect to.
- `{opts?}` (`table?`) Options (see above).

**Returns:** ([`maki.net.Conn?`](#maki-net-Conn), `string?`) The connection, or nil plus an error string.

**Example:**

```lua
maki.async.spawn(function()
  local conn, err = maki.net.connect("127.0.0.1", 7777)
  if not conn then return maki.log.error(err) end
  conn:write("hello\n")
  while true do
    local chunk = conn:read()
    if not chunk then break end
    handle(chunk)
  end
  conn:close()
end)
```


## maki.net.Conn {#maki-net-Conn}

A TCP connection opened by `maki.net.connect`.

`read` and `write` yield until done and can run at the same time.
The connection closes on `:close()`, when the handle is garbage
collected, and when the plugin unloads.

---

### `Conn:read()` {#Conn-read}

```lua
Conn:read()
```

Wait for data and return what arrived, at most 64 KiB. Returns
`nil, nil` once the peer has closed its side.

Only one read at a time: a second `read()` while one is waiting returns
an error. A read and a write can run at the same time.

**Returns:** (`string?`, `string?`) Bytes read, or nil plus an error string, or nil, nil at end of stream.

**Example:**

```lua
local chunk, err = conn:read()
if err then return maki.log.error(err) end
if not chunk then print("peer closed") end
```

---

### `Conn:write()` {#Conn-write}

```lua
Conn:write({data})
```

Send {data} and wait until all of it is written. Writes made while one
is in flight wait their turn, so each goes out whole and in call order.
A write that is cancelled or fails partway closes the connection,
because the peer would read the next write as the rest of the cut one.

**Parameters:**

- `{data}` (`string`) Bytes to send.

**Returns:** (`boolean?`, `string?`) `true`, or nil plus an error string.

**Example:**

```lua
local ok, err = conn:write(maki.json.encode(msg) .. "\n")
if not ok then return maki.log.error(err) end
```

---

### `Conn:close()` {#Conn-close}

```lua
Conn:close()
```

Close the connection. A read or write in flight ends with an error.
Extra calls do nothing.


## maki.provider {#maki-provider}

Providers implemented in Lua.

A registered provider works like a built-in one: its models show up in
the picker, and its requests get the usual retries, pricing and usage
accounting. The [Providers guide](/docs/providers/#plugin-providers)
covers writing one.

```lua
maki.provider.register({
  slug = "acme",
  display_name = "Acme",
  codec = "openai",
  base_url = "https://api.acme.com/v1",
  api_key_env = "ACME_API_KEY",
  models = { { prefixes = { "acme-large" }, tier = "strong" } },
})
```

---

### `maki.provider.register()` {#maki-provider-register}

```lua
maki.provider.register({spec})
```

Register a provider this plugin implements. Its models are addressed as
`<slug>/<model>` and appear in the model picker and in `/model`.

Call it at the top level of the plugin file, since registration only works
while the plugin loads. The plugin needs a non-empty `net_hosts` list in its
`plugin.toml`. The [Providers guide](/docs/providers/#plugin-providers)
walks through a full example.

A slug models.dev serves has to be one the package was granted: the
manifest lists it under `providers`, and the install approval covers it.
List every slug the package registers, so a name the catalog adds later
is already approved. A slug maki ships is reserved. See
[granted slugs](/docs/providers/#granted-slugs).

Set exactly one of `codec` or `base`. An unknown key, or an option the
chosen codec cannot honour, fails registration.

Every hook is optional and gets a `ctx` table as its first argument:
- `ctx.slug` (`string`) The slug the hook serves.
- `ctx.base_url` (`string?`) The origin requests go to right now: an origin
  `auth` returned, then `<SLUG>_BASE_URL` or `providers.toml`,
  then the declared `base_url`, nil when none is set. Build URLs
  from it so side calls follow a user who points the slug at a
  gateway.
- `ctx.headers` (`table`) The headers every request to the slug carries.
- `ctx.get_json(target)` (`function`) A GET with `ctx.headers`. A `target`
  starting with `/` is appended to `ctx.base_url`, an absolute URL
  is used as is. Never retried. Returns the decoded body, or nil
  plus an error the hook can return as its own.

A hook fails by returning `nil, err`, with `err` from `ctx.get_json` or
`maki.provider.http_error`. Maki then retries and honours `retry-after` as
it does for a built-in provider.

{spec} fields:
- `slug` (`string`) Required. Letters, digits, `_` and `-`, starting with a
  letter or digit. See the [slug rules](/docs/providers/#slug-rules).
- `display_name` (`string`) Required. Shown in the UI.
- `codec` (`string`) Wire format: `"openai"`, `"openai-responses"`,
  `"anthropic"` or `"google"`.
- `base` (`string`) A native provider to borrow whole, e.g. `"ollama"`.
  Prefer `codec` for a new provider.
- `base_url` (`string`) Default origin. Must be `https`, or `http` on
  loopback, and its host must match `net_hosts`. Only with
  `codec`. A `base` moves only to an origin the `auth` hook
  returns, so plans with a `base_url` need a `codec` too.
- `api_key_env` (`string`) Env var holding the API key, re-read each time
  the provider is built. Sent as `x-api-key` for anthropic,
  `x-goog-api-key` for google, and a bearer token otherwise.
  Also lists the provider in `maki auth login`, which saves the
  key. Needs the `env` permission.
- `default_model` (`string`) Model id without the slug, selected after
  `maki auth login`.
- `login_url` (`string`) Page `maki auth login` opens to get a key.
- `plans` (`table`) List of `{ key, display_name, base_url, default_model,
  login_url }` for `maki auth login` to offer. `key` and
  `display_name` are required. A plan's `base_url` defaults to
  the provider's and must match `net_hosts`. The choice is saved
  as `plan` in `providers.toml`.
- `family` (`string`) `"generic"`, `"claude"`, `"gpt"`, `"gemini"`,
  `"glm"` or `"synthetic"`. Applies to models without a row.
  Defaults to the provider behind `codec` or `base`.
- `accepts_arbitrary_models` (`boolean`) Assign tiers to `list_models`
  results. When false, tiers come only from `models`. This and
  the next two default to the `base` provider's, or with a
  `codec` to `true`, `16384` and `128000`.
- `max_output_tokens` (`integer|false`) Output cap for rows that leave it
  out and models without a row. `false` sends no cap.
- `context_window` (`integer`) Context window for rows that leave it out
  and models without a row.
- `pricing_schedule` (`table`) Peak-hour pricing, as
  `{ windows = { { 1, 4 }, ... }, multiplier = 2,
  weekdays_only = true }`. Windows are `{ start, end }` UTC
  hours with `end` exclusive. Row `pricing` is the off-peak rate.
- `aperture` (`table`) `{ path_prefix = "/v1" }` routes Aperture models
  of this slug through this provider.
- `docs` (`table`) `{ features, discovery_note }` for the generated
  [Providers](/docs/providers/) page. `discovery_note` replaces
  the model table when `models` is empty.
- `system_prefix` (`string`) Text prepended to the system prompt. The
  `google` codec refuses it.
- `openai` (`table`) Options for `codec = "openai"`, all optional:
  - `max_tokens_field` (`string`) Body field carrying the output cap.
    Defaults to `max_tokens`.
  - `include_stream_usage` (`boolean`) Ask for usage on the stream.
    Defaults to `true`.
  - `thinking` (`table`) How the API spells reasoning effort. Without it,
    each model's `thinking_fields` decides.
    - `dialect` (`string`) Required. One of `"standard"`, `"codex"`,
      `"codex-5-1"`, `"coding-plan"`, `"gpt-5-6"`, `"gpt-6"`,
      `"prefer-high"`, `"high-only"`, `"glm"`, `"deepseek"`,
      `"anthropic-adaptive"`, `"gemini"`, `"tensorx"`, `"grok"`
      or `"ollama"`.
    - `field` (`string`) Body path for the effort. Dots nest, e.g.
      `"reasoning.effort"`. Defaults to `reasoning_effort`.
    - `requires_support` (`boolean`) Send effort only to models that
      support thinking. Defaults to `false`.
  - `headers` (`table`) Sent with every request. A header the credentials
    set wins. `host`, `content-length`, `transfer-encoding` and
    `connection` are refused.
  - `extra_body` (`table`) Merged into every request body.
  - `session_id` (`table`) Sends the session id, as
    `{ header = "x-affinity" }`, `{ body_field = "prompt_cache_key" }`,
    or both at once.
  - `thinking_overrides` (`table`) Model id prefix to `"no"`, `"yes"` or
    `"required"`, overriding the model table. Longest prefix wins.
- `models` (`table`) Static model rows, read once at registration. They
  describe models and add to the runtime list. See
  [model rows](/docs/providers/#model-rows).
- `auth` (`function`) `function(ctx, purpose)` returning
  `{ base_url = ..., headers = { ... } }`. `purpose` is
  `"resolve"` before the first request, `"refresh"` after a 401
  on the newest credentials, or `"reload"` to re-read stored
  credentials after a login or another maki process changed them.
  Spend a refresh token only on `"refresh"`.
  Omitting `base_url` keeps the current one.
- `list_models` (`function`) `function(ctx)` returning model rows for a
  catalogue only known at runtime. Without it, the provider
  lists what its codec or base lists. Rows carry `id`,
  `context_window`, `max_output_tokens`, `pricing`,
  `supports_thinking`, `supports_vision` and `tier`, plus two
  optional fields. `extra` is any JSON value, handed back to
  `build_body` as `opts.model_info`. `effort` narrows the
  `openai.thinking` dialect for this model: `supported` lists the
  effort names the provider accepts, and `send_off` is `true` to
  send `"none"` for off or `false` to send nothing.
- `build_body` (`function`) `function(ctx, body, model, opts)` returning
  the body to send. `opts.thinking` is the rendered effort level,
  nil when thinking is off. `openai` codecs only.
- `map_error` (`function`) `function(ctx, status, message)` returning
  `{ status = ..., message = ... }`, or nil to keep the error.
  Retryability follows the returned status.
- `fetch_usage` (`function`) `function(ctx)` returning
  `{ plan = ..., limits = { { label = ..., percentage = ...,
  reset_at = ..., detail = ... } }, by_model_today = { { model = ...,
  input_tokens = ..., output_tokens = ..., total_tokens = ...,
  spend_microdollars = ... } } }` or nil. Only `label` is
  required in a limit, and `by_model_today` is optional.
- `login` (`function`) `function(ctx)`. Defining it lists the provider in
  `maki auth login`. This `ctx` also has `ctx.print(text)`,
  `ctx.prompt({ label = ..., secret = ... })` and
  `ctx.open_url(url)`.
- `logout` (`function`) `function(ctx)`, run by `maki auth logout` before
  Maki deletes the stored credentials itself. Only needed for
  work Maki cannot do, such as revoking a token upstream.

Requires the `net` [plugin permission](#plugin-permissions).

**Parameters:**

- `{spec}` (`table`) Provider specification (see above).

**Example:**

```lua
maki.provider.register({
  slug = "acme",
  display_name = "Acme",
  codec = "openai",
  base_url = "https://api.acme.com/v1",
  models = {
    { prefixes = { "acme-large" }, tier = "strong", context_window = 200000 },
  },
  auth = function(ctx)
    local creds = maki.provider.auth.get(ctx.slug) or {}
    return { headers = { Authorization = "Bearer " .. (creds.token or "") } }
  end,
})
```

---

### `maki.provider.is_selected()` {#maki-provider-is_selected}

```lua
maki.provider.is_selected({slug})
```

Whether this package was granted {slug}: the manifest lists it under
`providers` and the install approval covers it. Kept from maki 0.6.3,
where multi-provider packages called it to skip the ones the user did not
select; it now answers from the same list.

**Parameters:**

- `{slug}` (`string`) Provider name.

**Returns:** boolean

---

### `maki.provider.http_error()` {#maki-provider-http_error}

```lua
maki.provider.http_error({res})
```

Turn a failed `maki.net.request` response into a provider error. A hook
returns it as `return nil, err`, and Maki handles it like a built-in
provider's failure: a 429 or 5xx is retried and `retry-after` sets the
wait. A hook that raises instead fails as a broken hook.

{res} fields:
- `status` (`integer`) Required. The HTTP status.
- `body` (`string`) Required. Becomes the error message.
- `headers` (`table`) Only `retry-after` is read, case-insensitively.

**Parameters:**

- `{res}` (`table`) A response from `maki.net.request`.

**Returns:** (`userdata`) A `ProviderError`. Opaque, but `tostring` renders it.

**Example:**

```lua
fetch_usage = function(ctx)
  local res = assert(maki.net.request(ctx.base_url .. "/usage", { headers = ctx.headers }))
  if res.status ~= 200 then
    return nil, maki.provider.http_error(res)
  end
  return { limits = {} }
end
```


## maki.provider.auth {#maki-provider-auth}

Credential storage for the providers this plugin registered.

Each slug gets one JSON file at
`~/.local/state/maki/auth/plugins/<slug>.json`, with mode 0600, atomic
writes and a lock against other Maki processes. The plugin decides what
goes in it. A plugin can only reach slugs it registered itself.

```lua
maki.provider.auth.set("acme", { access_token = tok, expires = when })
local creds = maki.provider.auth.get("acme")
maki.provider.auth.clear("acme")
```

---

### `maki.provider.auth.get()` {#maki-provider-auth-get}

```lua
maki.provider.auth.get({slug})
```

Read the credentials this plugin stored for one of its providers. Returns
nil when nothing was stored yet, for example before the first login.

**Parameters:**

- `{slug}` (`string`) A provider slug this plugin registered.

**Returns:** (`table?`, `string?`) The stored credentials, or nil plus an error.

**Example:**

```lua
local creds = maki.provider.auth.get("acme")
if creds then print(creds.access_token) end
```

---

### `maki.provider.auth.set()` {#maki-provider-auth-set}

```lua
maki.provider.auth.set({slug}, {credentials})
```

Store credentials for one of this plugin's providers, replacing what was
there. Any table with string keys works, such as a token plus its expiry.
An `auth` hook can call it to save a refreshed token.

**Parameters:**

- `{slug}` (`string`) A provider slug this plugin registered.
- `{credentials}` (`table`) Any table with string keys.

**Returns:** (`boolean?`, `string?`) True, or nil plus an error string.

**Example:**

```lua
local ok, err = maki.provider.auth.set("acme", { access_token = token })
if not ok then maki.log.error(err) end
```

---

### `maki.provider.auth.clear()` {#maki-provider-auth-clear}

```lua
maki.provider.auth.clear({slug})
```

Forget the credentials stored for one of this plugin's providers.

**Parameters:**

- `{slug}` (`string`) A provider slug this plugin registered.

**Returns:** (`boolean?`, `string?`) True, or nil plus an error string.

**Example:**

```lua
maki.provider.auth.clear("acme")
```


## maki.session {#maki-session}

Host session primitives. The interactive UI can run several sessions
at once. These functions list, create, focus, rename, and delete them.

Without a UI, most functions return `nil, "no interactive UI attached"`.
`current`, `read`, `export`, `messages`, `notify` and `new` without
`prompt` or `focus` still work. ACP has no focused session, so there
`current` fails and `read`, `export` and `messages` need `session`.

---

### `maki.session.list()` {#maki-session-list}

```lua
maki.session.list()
```

Lists sessions stored for the current project. Answered from a
background scan, so a slow disk never blocks the UI.

**Returns:** (`table|nil`, `string|nil`) Array of `{id, title, updated_at}`, or nil and an error.

**Example:**

```lua
local stored, err = maki.session.list()
```

---

### `maki.session.live()` {#maki-session-live}

```lua
maki.session.live()
```

Lists the sessions currently running in this UI. Status is "working",
"needs_input", or "idle". A mailbox follow-up stays "working" without an
intermediate "idle" status.

**Returns:** (`table|nil`, `string|nil`) Array of `{id, title, status, updated_at, focused}`, or nil and an error.

**Example:**

```lua
local live, err = maki.session.live()
```

---

### `maki.session.current()` {#maki-session-current}

```lua
maki.session.current()
```

Returns the id of the focused session. Under `maki -p` and the sdk,
that is the one session they run.

**Returns:** (`string|nil`, `string|nil`) Session id, or nil and an error.

**Example:**

```lua
local id = maki.session.current()
```

---

### `maki.session.read()` {#maki-session-read}

```lua
maki.session.read({opts?})
```

One-call snapshot of a session: queue, usage, context, cost, mode, and
status. Reads the focused session, or the one you name in `session`: a
background tab, or a stored session nothing has open.

Metadata only, so it needs no permission. Not to be confused with the
`session_read` permission, which guards transcript content
(`maki.session.export`, `maki.session.messages`).

The returned table:
```text
{
  id, cwd, model, mode = "build" | "plan",
  thinking, fast,    -- nil under headless drivers
  status = "idle" | "working" | "needs_input" | "stored",
  focused, updated_at,
  usage = { input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens },
  context_size,
  context_window,    -- nil for a stored session
  cost,              -- nil for a stored session
  queue = { count }, -- nil under headless drivers and for a stored session
  title,             -- nil under headless drivers
}
```

`status` is `"stored"` for a session nothing here runs. Its `model` is the
one it was saved with, resolved only when it opens.

`usage` and `cost` include subagent spend. `context_size` is the main
session's own, since a subagent runs its own window. There is no
`list_cost` here: the un-subsidised total for a run arrives on the
`TurnEnd` autocmd, and `maki.model.info` carries the rates behind it.

**Parameters:**

- `{opts?}` (`table?`) `session` (string?) Session id, live or stored. Defaults to the focused one.

**Returns:** (`table|nil`, `string|nil`) Snapshot table, or nil and an error.

**Example:**

```lua
local s = maki.session.read()
if s.context_size > s.context_window * 0.8 then
  maki.notify("context is nearly full")
end
```

---

### `maki.session.export()` {#maki-session-export}

```lua
maki.session.export({opts?})
```

Reads a session as the document `maki session export` prints, the same
table `maki.session.new` takes back:

```text
{ format, model, mode, thinking, fast, title, facts, messages }
```

`messages` is what `maki.session.messages` returns. `facts` is the date,
working directory, model, instructions and hints the model was last told,
nil when the session never ran. A live session is read from memory,
unsaved messages included, any other from disk without claiming it, so a
session another maki has open can be read too.

JSON null comes back as `maki.NIL` and arrays are tagged, so
`maki.session.export(...)` encodes to the same document as the CLI file.
Copy it with `maki.deepcopy` before editing, which keeps both intact.

Requires the `session_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{opts?}` (`table?`) `session` (string?) Session id, live or stored. Defaults to the focused one.

**Returns:** (`table|nil`, `string|nil`) The document, or nil and an error.

**Example:**

```lua
local doc = assert(maki.session.export({ session = maki.session.current() }))
maki.fs.write("/tmp/session.json", maki.json.encode(doc))
```

---

### `maki.session.messages()` {#maki-session-messages}

```lua
maki.session.messages({opts?})
```

Reads a session's transcript, oldest first: everything the model has
been sent so far, tool calls and results included. Read only.

Each message has the same shape as in the session file:

```text
{ role = "user" | "assistant", kind, facts?, display_text?, content = { blocks } }
```

`kind` is `"turn"` for something the user or the model said,
`"observation"` for a report sent to the model as a user message, like
`maki.session.notify`, and `"context_update"` for a change since the
system prompt was built (date, model, plan mode, ...), which carries the
changed `facts`. `display_text` is what the UI shows instead of the
content: nil shows the content, `""` hides the message (a nudge, a
compaction note). `content` lists blocks:

```text
{ type = "text", text }
{ type = "thinking", thinking, signature? }
{ type = "redacted_thinking", data }
{ type = "tool_use", id, name, input, thought_signature? }
{ type = "tool_result", tool_use_id, content, is_error?, loaded_tools? }
{ type = "image", source = maki.image.Image }
```

`signature`, `thought_signature`, `data` and `loaded_tools` are opaque:
pass them through untouched. A block type this build does not know comes
as its raw table. JSON null is `maki.NIL`.

A live session is read from memory, unsaved messages included, any other
from disk. Works in the TUI, `maki -p`, sdk mode, and ACP. ACP has no
focused session, so pass `session` there. Hook and event payloads carry
the `session_id` to pass.

Requires the `session_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{opts?}` (`table?`) Options:
  - `session` (`string?`) Session id, live or stored. Defaults to the focused one.
  - `last` (`integer?`) only the newest `last` messages.

**Returns:** (`table|nil`, `string|nil`) Array of messages, or nil and an error.

**Example:**

```lua
local msgs = maki.session.messages({ last = 1 })
local last = msgs and msgs[1]
if last and last.role == "assistant" and last.content[1].type == "text" then
  print(last.content[1].text)
end
```

---

### `maki.session.focus()` {#maki-session-focus}

```lua
maki.session.focus({id})
```

Switches the UI to the session with {id}.

**Parameters:**

- `{id}` (`string`) Session id, as returned by `list()` or `live()`.

**Returns:** (`boolean|nil`, `string|nil`) true on success, or nil and an error.

**Example:**

```lua
local _, err = maki.session.focus(id)
```

---

### `maki.session.delete()` {#maki-session-delete}

```lua
maki.session.delete({id})
```

Deletes a session and its stored history, cancelling it first if it
is running. The focused session cannot be deleted.

Requires the `session_write` [plugin permission](#plugin-permissions).

**Parameters:**

- `{id}` (`string`) Session id to delete.

**Returns:** (`boolean|nil`, `string|nil`) true on success, or nil and an error.

**Example:**

```lua
local _, err = maki.session.delete(id)
```

---

### `maki.session.new()` {#maki-session-new}

```lua
maki.session.new({opts?})
```

Creates a session from the document `maki.session.export` returns, or
from nothing.

Whatever the document leaves out is what a new tab would get: in the TUI
the focused tab's project, model, mode, thinking, fast and workflow, under
`maki -p` and sdk mode the project of the session they run, under ACP the
directory maki runs in. Outside the TUI no model is stored, so the session
opens on the default model. A document with messages gets no permissions
(`yolo`, session rules) from either side. Without messages it is a new tab
like `Ctrl-N`, which keeps the focused tab's `yolo`.

Without `prompt` or `focus` the session is only stored, and
`maki.session.focus(id)` opens it later. With either it opens in a tab.
A session is only stored once it has messages, so storing a document
with none left after repair fails. Pass `focus` to open a blank one
instead.
Headless drivers (`maki -p`, ACP) have nothing to open it in, so there
`prompt` and `focus` answer `nil, "no interactive UI attached"` and store
nothing.

History is repaired the way a resume repairs it (orphan tool results go,
a dangling final tool call is closed) and anything else wrong is refused
with its location, like `messages[12].content[2]: ...` (0-based, as in
the JSON document). A block of a known type that does not parse, or one
without a `type`, is refused too, while one of a type this build does not
know is kept. Thinking blocks and `thought_signature` are dropped: they
are bound to a system prompt the new session will not have. `model` is
stored as given and resolved when the session opens, falling back with a
warning like any resume.

Image sources may be a `maki.image.Image` or the
`{ type = "base64", media_type, data }` table `maki.json` gives. An empty
table where a list belongs (`messages`, a message's `content`) is refused,
since nothing tells it from an empty object: copy with `maki.deepcopy`,
which keeps the tag. Fields `new` does not know are ignored.

Requires the `session_write` [plugin permission](#plugin-permissions).

**Parameters:**

- `{opts?}` (`table?`) Optional fields:
  - `messages` (`table`) what `export` or `messages` return. A user turn
    that starts with `/` or `!`, or is `exit`, needs the `agent`
    permission too, since a rewind puts it back in the input for Enter
    to run.
  - `facts` (`table`) what `export` returns: the date, working directory,
    model, instructions and hints the model was last told.
  - `model` (`string`) model spec. Defaults to the focused tab's in the TUI,
    else the default model when the session opens.
  - `mode` (`string`) "build" or "plan".
  - `thinking` (`string`) spelled like the thinking setting.
  - `fast` (`boolean`) fast output mode, where the model supports it.
  - `title` (`string`) else generated from the history.
  - `prompt` (`string`) first user message to submit right away. Needs the
    `agent` permission.
  - `focus` (`boolean`) switch the UI to the new session.

**Returns:** (`string|nil`, `string|nil`) New session id, or nil and an error.

**Example:**

```lua
-- copy a session, keeping it in the background
local doc = assert(maki.session.export({ session = id }))
local copy, err = maki.session.new(doc)

-- start a fresh one right away
local id, err = maki.session.new({ prompt = "fix the tests", focus = true })
```

---

### `maki.session.prompt()` {#maki-session-prompt}

```lua
maki.session.prompt({text}, {opts?})
```

Sends {text} as a regular user prompt to a live session. The text is
never interpreted: slash commands, `exit`, and `!` shell prefixes are
all sent to the model verbatim. If the session is currently streaming,
the prompt is queued and picked up when the agent reaches it.

Requires the `agent` and `session_write` [plugin permissions](#plugin-permissions).

**Parameters:**

- `{text}` (`string`) The prompt to send. Must not be blank.
- `{opts?}` (`table?`) Optional fields: session (string) id of a live

  session; defaults to the focused one.


**Returns:** (`string|nil`, `string|nil`) "started" or "queued", or nil and an error.

**Example:**

```lua
local state, err = maki.session.prompt("run the tests", { session = id })
```

---

### `maki.session.notify()` {#maki-session-notify}

```lua
maki.session.notify({text}, {opts?})
```

Reports {text} to a live session without creating a user turn. The
observation waits for the session's next agent run.

Requires the `session_write` [plugin permission](#plugin-permissions).

**Parameters:**

- `{text}` (`string`) What to report. Must not be blank.
- `{opts?}` (`table`) Options:
  - `session` (`string`) id of a live session.
  - `wake` (`boolean`) start a TUI turn when it next becomes idle (default
    false). Needs the `agent` permission.
  - `display` (`string?`) one-line note for the transcript, shown and saved
    when the session's agent run accepts the report. Whitespace collapses
    to single spaces and the note is cut to 256 bytes. Defaults to
    nothing; the model reads only {text}.

**Returns:** (`boolean|nil`, `string|nil`) true, or nil and an error.

**Example:**

```lua
maki.session.notify("[monitor] deploy failed", { session = id, wake = true })
```

---

### `maki.session.set_mode()` {#maki-session-set_mode}

```lua
maki.session.set_mode({mode}, {opts?})
```

Switches a live session between plan and build mode. Entering plan mode
allocates the session's plan file if it has none.

A session that is mid-plan answers the next prompt with another draft of
the plan. Set `"build"` first and that prompt implements it.

Requires the `session_settings` [plugin permission](#plugin-permissions).

**Parameters:**

- `{mode}` (`string`) "build" or "plan".
- `{opts?}` (`table?`) Options:
  - `session` (`string`) id of a live session, defaults to the focused one.

**Returns:** (`boolean|nil`, `string|nil`) true, or nil and an error.

**Example:**

```lua
maki.session.set_mode("build", { session = opts.session })
maki.session.prompt("Implement the plan at `" .. opts.path .. "`.", { session = opts.session })
```

---

### `maki.session.set_title()` {#maki-session-set_title}

```lua
maki.session.set_title({opts})
```

Renames a session, live or stored.

Requires the `session_settings` [plugin permission](#plugin-permissions).

**Parameters:**

- `{opts}` (`table`) Required fields: id (string) session to rename;
  - `title` (`string`) the new title.

**Returns:** (`boolean|nil`, `string|nil`) true on success, or nil and an error.

**Example:**

```lua
local _, err = maki.session.set_title({ id = id, title = "refactor" })
```


## maki.Timer {#maki-Timer}

Handle returned by `maki.defer_fn`. Its `:stop()` cancels the
callback before it fires, which is what debouncing is built on.

---

### `Timer:stop()` {#Timer-stop}

```lua
Timer:stop()
```

Cancel the pending callback. Safe to call more than once, and does
nothing once the callback has already run.

**Example:**

```lua
local h = maki.defer_fn(function() rebuild() end, 300)
h:stop()
```


## maki.task {#maki-task}

The subagents of the focused session and their transcripts. Tasks are
spawned by the `task` tool and addressed by an id that survives a reload.
Without an interactive UI every function returns
`nil, "no interactive UI attached"`.

---

### `maki.task.list()` {#maki-task-list}

```lua
maki.task.list()
```

Lists the focused session's chats in chat order. Entry 1 is always the main
chat, with id `"main"` and no `status`: its work is the session's own, and
`maki.session.live()` already reports that. The rest are subagents: one a
tool spawned has its tool call's id, one a plugin opened with
[maki.agent.session](#maki-agent-session) has its session id and a
`plugin` field.

**Returns:** (`table|nil`, `string|nil`) Array of `{id, session, name, focused, status?, plugin?}`
  where `status` is `"working"`, `"idle"` (a plugin task between prompts),
  `"done"`, or `"error"`, and `session` is the chat session that `stop()`
  asks for. Or nil and an error.

**Example:**

```lua
for _, t in ipairs(maki.task.list() or {}) do
  print(t.name, t.status or "main")
end
```

---

### `maki.task.focus()` {#maki-task-focus}

```lua
maki.task.focus({id})
```

Shows a task's transcript, the way the chat cycling keys do. An id from
another session returns an error instead of landing on the wrong task.

**Parameters:**

- `{id}` (`string`) Task id, as returned by `list()`. `"main"` is the main chat.

**Returns:** (`boolean|nil`, `string|nil`) true on success, or nil and an error.

**Example:**

```lua
local _, err = maki.task.focus("main")
```

---

### `maki.task.remove()` {#maki-task-remove}

```lua
maki.task.remove({id})
```

Deletes a finished task and its saved transcript. Refuses the main chat and
running tasks. Deleting the focused task moves focus to the chat before it.

Requires the `session_write` [plugin permission](#plugin-permissions).

**Parameters:**

- `{id}` (`string`) Task id, as returned by `list()`.

**Returns:** (`boolean|nil`, `string|nil`) true on success, or nil and an error.

**Example:**

```lua
local _, err = maki.task.remove("toolu_01")
```

---

### `maki.task.stop()` {#maki-task-stop}

```lua
maki.task.stop({id}, {opts})
```

Stops a task in the focused session, the same way Esc in its tab does. A
plugin task can be stopped while idle, even before its first prompt. The
main chat, a finished task, or an unknown id returns an error.

Only the user may stop a task, so this works in a command the user typed
or a key they pressed. Tools, hooks, autocmds, `maki.async` tasks, and
commands run through `maki.api.run_command` get an error instead, or any
plugin could stop work that is not its own.

**Parameters:**

- `{id}` (`string`) Task id, as returned by `list()`.
- `{opts}` (`table`) `session` (string), the chat session of the `list()`

  row. If the user switched sessions since, this returns an error rather


  than stopping a task with the same id in the other session.


**Returns:** (`boolean|nil`, `string|nil`) true on success, or nil and an error.

**Example:**

```lua
local _, err = maki.task.stop(task.id, { session = task.session })
```


## maki.text {#maki-text}

Text utilities: format conversion and the fuzzy matcher the built-in
pickers use.

```lua
local md = maki.text.html_to_markdown(html)
local hits = maki.text.fuzzy_list("mrs", names, { limit = 10 })
```

---

### `maki.text.html_to_markdown()` {#maki-text-html_to_markdown}

```lua
maki.text.html_to_markdown({html})
```

Convert an HTML string to Markdown.
Useful for cleaning up web content fetched with `maki.webfetch`.

**Parameters:**

- `{html}` (`string`) HTML source text.

**Returns:** (`string?`, `string?`) Markdown text on success, or nil plus an error message.

**Example:**

```lua
local md, err = maki.text.html_to_markdown("<h1>Hello</h1><p>world</p>")
if err then return end
print(md) -- "# Hello\n\nworld"
```

---

### `maki.text.fuzzy()` {#maki-text-fuzzy}

```lua
maki.text.fuzzy({needle}, {haystack}, {opts?})
```

Scores {needle} against {haystack} with the fuzzy matcher the built-in
pickers use. {needle} is one pattern, spaces included.

Higher is better. Scores are only comparable across haystacks scored
against the same needle. An empty needle matches everything with score 0.

The second return value lists where the match landed, in the same shape
as `maki.fs.fuzzy_files`: 1-based inclusive `{ from, to }` byte ranges,
ascending, with adjacent characters merged. `haystack:sub(from, to)` is
the matched text.

Needs no plugin permission.

**Parameters:**

- `{needle}` (`string`) What the user typed.
- `{haystack}` (`string`) The candidate to score it against.
- `{opts?}` (`table?`) Options:
  - `paths` (`boolean`) rank {haystack} as a path, favouring the last segment, like the file picker. Off by default, like the model, command and list pickers.

**Returns:** (`integer|nil`, `table|nil`) Score and matched byte ranges, or nil when the needle does not match.

**Example:**

```lua
local score, at = maki.text.fuzzy("mrs", "maki-ui/src/main.rs", { paths = true })
if score then print(("maki-ui/src/main.rs"):sub(at[1][1], at[1][2])) end
```

---

### `maki.text.fuzzy_list()` {#maki-text-fuzzy_list}

```lua
maki.text.fuzzy_list({needle}, {haystacks}, {opts?})
```

Scores {needle} against every entry of {haystacks} and returns the
matches, best first.

Ties keep their input order, so candidates you pre-sorted (by mtime, say)
stay in that order for an empty needle. `index` is the 1-based position in
{haystacks}, and `highlights` uses the byte ranges of `fuzzy`. Entries
that are not valid UTF-8 are skipped.

To rank files, use `maki.fs.fuzzy_files` instead. It queries the index
the host already keeps, so no candidate list crosses into Lua.

Needs no plugin permission.

**Parameters:**

- `{needle}` (`string`) What the user typed.
- `{haystacks}` (`table`) Array of candidate strings.
- `{opts?}` (`table?`) Options:
  - `limit` (`integer`) keep at most this many results.
  - `paths` (`boolean`) rank candidates as paths, like the file picker. Off by default.
  - `highlights` (`boolean`) also return where the query matched, off by default since it costs a second pass.

**Returns:** (`table`) Array of `{ text, index, score, highlights? }`, best first.

**Example:**

```lua
for _, m in ipairs(maki.text.fuzzy_list(query, names, { limit = 10 })) do
  print(m.text, m.score)
end
```


## maki.treesitter {#maki-treesitter}

Tree-sitter parsing and query API.

Mirrors `vim.treesitter` from Neovim, so plugins can be shared between the two.
Start with `get_parser()` to parse source code, then use `get_node_text()` and
the `query` sub-module to extract information from the syntax tree.

```lua
local parser, err = maki.treesitter.get_parser(source, "lua")
local trees = parser:parse()
local root = trees[1]:root()
```

---

### `maki.treesitter.get_parser()` {#maki-treesitter-get_parser}

```lua
maki.treesitter.get_parser({source}, {lang})
```

Creates a `LanguageTree` for {source} using the grammar named {lang}.
This is the main entry point for parsing source code with tree-sitter.
Signature matches `vim.treesitter.get_parser()`, so Neovim plugins can be copy-pasted.

**Parameters:**

- `{source}` (`string`) Source text to parse.
- `{lang}` (`string`) Language name, e.g. `"rust"` or `"lua"`.

**Returns:** ([`LanguageTree|nil`](#maki-treesitter-LanguageTree), `string|nil`) Parser, or nil and an error message.

**Example:**

```lua
local parser, err = maki.treesitter.get_parser(src, "lua")
if err then print("error: " .. err) end
```

---

### `maki.treesitter.get_string_parser()` {#maki-treesitter-get_string_parser}

```lua
maki.treesitter.get_string_parser({source}, {lang})
```

Alias for `get_parser`. Use whichever name you prefer.

**Parameters:**

- `{source}` (`string`) Source text to parse.
- `{lang}` (`string`) Language name.

**Returns:** ([`LanguageTree|nil`](#maki-treesitter-LanguageTree), `string|nil`) Parser, or nil and an error message.

---

### `maki.treesitter.get_node_text()` {#maki-treesitter-get_node_text}

```lua
maki.treesitter.get_node_text({node}, {source})
```

Gets the text that {node} covers in {source}.
Useful when you have a captured node and need the actual source substring.

**Parameters:**

- `{node}` ([`Node`](#maki-treesitter-Node)) The node whose text you want.
- `{source}` (`string`) Original source text the tree was parsed from.

**Returns:** (`string`) Substring covered by the node.

**Example:**

```lua
local text = maki.treesitter.get_node_text(node, source)
print(text)
```

---

### `maki.treesitter.get_node_range()` {#maki-treesitter-get_node_range}

```lua
maki.treesitter.get_node_range({node})
```

Returns the range of {node} as four 0-based integers: start_row, start_col, end_row, end_col.

**Parameters:**

- `{node}` ([`Node`](#maki-treesitter-Node)) The node to query.

**Returns:** (`integer`, `integer`, `integer`, `integer`) start_row, start_col, end_row, end_col.

**Example:**

```lua
local sr, sc, er, ec = maki.treesitter.get_node_range(node)
```

---

### `maki.treesitter.get_range()` {#maki-treesitter-get_range}

```lua
maki.treesitter.get_range({node})
```

Returns a six-element table for {node}: `{start_row, start_col, start_byte, end_row, end_col, end_byte}`.
This gives you byte offsets in addition to row/column positions.

**Parameters:**

- `{node}` ([`Node`](#maki-treesitter-Node)) The node to query.

**Returns:** (`table`) Six-element array: start_row, start_col, start_byte, end_row, end_col, end_byte.

**Example:**

```lua
local r = maki.treesitter.get_range(node)
print("bytes: " .. r[3] .. "-" .. r[6])
```

---

### `maki.treesitter.is_ancestor()` {#maki-treesitter-is_ancestor}

```lua
maki.treesitter.is_ancestor({dest}, {source})
```

Checks whether {dest} is an ancestor of {source} (or the same node).
Walks up from {source} toward the root looking for {dest}.

**Parameters:**

- `{dest}` ([`Node`](#maki-treesitter-Node)) Potential ancestor node.
- `{source}` ([`Node`](#maki-treesitter-Node)) Node to check ancestry for.

**Returns:** (`boolean`)

---

### `maki.treesitter.is_in_node_range()` {#maki-treesitter-is_in_node_range}

```lua
maki.treesitter.is_in_node_range({node}, {line}, {col})
```

Checks whether the 0-based position ({line}, {col}) falls inside {node}.
Handy for cursor-position checks.

**Parameters:**

- `{node}` ([`Node`](#maki-treesitter-Node)) Node to test against.
- `{line}` (`integer`) 0-based line number.
- `{col}` (`integer`) 0-based column number.

**Returns:** (`boolean`)

---

### `maki.treesitter.node_contains()` {#maki-treesitter-node_contains}

```lua
maki.treesitter.node_contains({node}, {range})
```

Checks whether {node} fully contains the given {range}.

**Parameters:**

- `{node}` ([`Node`](#maki-treesitter-Node)) Node to test.
- `{range}` (`table`) Four-element array `{start_row, start_col, end_row, end_col}`.

**Returns:** (`boolean`)

---

### `maki.treesitter.get_node()` {#maki-treesitter-get_node}

```lua
maki.treesitter.get_node({opts?})
```

Placeholder for cursor-based node lookup (not yet implemented, always returns nil).

**Parameters:**

- `{opts?}` (`table?`) Options (currently unused).

**Returns:** ([`Node|nil`](#maki-treesitter-Node)) Always nil.


## maki.treesitter.language {#maki-treesitter-language}

Language registry for tree-sitter grammars.

Mirrors `vim.treesitter.language`. Use these functions to register grammars,
map filetypes to languages, and inspect available node types.

```lua
maki.treesitter.language.add("lua")
maki.treesitter.language.register("lua", "luau")
```

---

### `maki.treesitter.language.add()` {#maki-treesitter-language-add}

```lua
maki.treesitter.language.add({lang}, {opts?})
```

Registers {lang} for use with tree-sitter.
Call this to confirm a language grammar is available. Like
`vim.treesitter.language.add`. Custom grammar paths are not yet supported.

**Parameters:**

- `{lang}` (`string`) Language name, e.g. `"rust"`.
- `{opts?}` (`table?`) Options table (the `path` key is not yet supported).

**Returns:** (`boolean?`, `string?`) `true`, or nil plus an error if {lang} is unknown.

**Example:**

```lua
if not maki.treesitter.language.add("lua") then return end
```

---

### `maki.treesitter.language.register()` {#maki-treesitter-language-register}

```lua
maki.treesitter.language.register({lang}, {filetype})
```

Associates {lang} with one or more filetypes, so you can look up the right
parser language for a given filetype later with `get_lang()`.

**Parameters:**

- `{lang}` (`string`) Language name.
- `{filetype}` (`string|table`) A single filetype string or an array of filetype strings.

**Example:**

```lua
maki.treesitter.language.register("tsx", { "tsx", "jsx" })
```

---

### `maki.treesitter.language.get_lang()` {#maki-treesitter-language-get_lang}

```lua
maki.treesitter.language.get_lang({filetype})
```

Looks up the tree-sitter language name for {filetype}.
Returns the registered language, or falls back to {filetype} itself if
a grammar with that name exists. Returns nil when nothing matches.

**Parameters:**

- `{filetype}` (`string`) Filetype to look up, e.g. `"ts"`.

**Returns:** (`string|nil`) Language name, or nil.

**Example:**

```lua
maki.treesitter.language.register("tsx", { "tsx", "jsx" })
local lang = maki.treesitter.language.get_lang("jsx") -- "tsx"
```

---

### `maki.treesitter.language.get_filetypes()` {#maki-treesitter-language-get_filetypes}

```lua
maki.treesitter.language.get_filetypes({lang})
```

Returns all filetypes that have been registered for {lang}.

**Parameters:**

- `{lang}` (`string`) Language name.

**Returns:** (`table`) Array of filetype strings.

**Example:**

```lua
local fts = maki.treesitter.language.get_filetypes("tsx")
-- { "tsx", "jsx" }
```

---

### `maki.treesitter.language.inspect()` {#maki-treesitter-language-inspect}

```lua
maki.treesitter.language.inspect({lang})
```

Returns metadata about the grammar for {lang}.
Useful for debugging or discovering which node types and fields a grammar defines.

**Parameters:**

- `{lang}` (`string`) Language name.

**Returns:** (`table`) Table with keys `abi_version` (integer), `node_types` (string[]), `fields` (string[]).

**Example:**

```lua
local info = maki.treesitter.language.inspect("lua")
print("ABI: " .. info.abi_version)
for _, nt in ipairs(info.node_types) do print(nt) end
```


## maki.treesitter.query {#maki-treesitter-query}

Query compilation and lookup.

Mirrors `vim.treesitter.query`. Use `parse()` to compile a tree-sitter
query string into a `Query` object you can run against parsed trees.

```lua
local q = maki.treesitter.query.parse("lua", "(string) @str")
```

---

### `maki.treesitter.query.parse()` {#maki-treesitter-query-parse}

```lua
maki.treesitter.query.parse({lang}, {query})
```

Compiles a tree-sitter query string for {lang}.
Throws if the language is unknown or the query has a syntax error.

**Parameters:**

- `{lang}` (`string`) Language name, e.g. `"lua"`.
- `{query}` (`string`) Tree-sitter S-expression query.

**Returns:** ([`Query`](#maki-treesitter-Query)) Compiled query object.

**Example:**

```lua
local q = maki.treesitter.query.parse("lua", "(identifier) @id")
```

---

### `maki.treesitter.query.get()` {#maki-treesitter-query-get}

```lua
maki.treesitter.query.get({lang}, {name})
```

Looks up a named built-in query for {lang} (not yet implemented, always returns nil).

**Parameters:**

- `{lang}` (`string`) Language name.
- `{name}` (`string`) Query name, e.g. `"highlights"`.

**Returns:** ([`Query|nil`](#maki-treesitter-Query)) Query object, or nil if not found.


## maki.treesitter.Query {#maki-treesitter-Query}

A compiled tree-sitter query.

Get one by calling `maki.treesitter.query.parse(lang, query_string)`.
Then use `:iter_captures()` or `:iter_matches()` to run it against a syntax tree.

```lua
local q = maki.treesitter.query.parse("lua", "(identifier) @id")
for idx, node, meta in q:iter_captures(root, source) do
  print(node:type())
end
```

---

### `Query:iter_captures()` {#Query-iter_captures}

```lua
Query:iter_captures({node}, {source}, {start_row?}, {stop_row?})
```

Iterates over every capture matched by this query. Each call to the returned iterator yields `(capture_index, node, metadata, match, active)`. Use this when you care about individual captures rather than whole pattern matches.

**Parameters:**

- `{node}` ([`Node`](#maki-treesitter-Node)) Root node to search within.
- `{source}` (`string`) Source text the tree was parsed from.
- `{start_row?}` (`integer`) Only match rows >= this value (0-based).
- `{stop_row?}` (`integer`) Only match rows < this value (0-based).

**Returns:** (`function`) Iterator yielding (integer, Node, table, table, integer).

**Example:**

```lua
local q = maki.treesitter.query.parse("lua", "(identifier) @id")
for idx, node, meta in q:iter_captures(root, source) do
  print(idx, node:type())
end
```

---

### `Query:iter_matches()` {#Query-iter_matches}

```lua
Query:iter_matches({node}, {source}, {start_row?}, {stop_row?})
```

Iterates over every full pattern match in this query. Each call to the returned iterator yields `(pattern_index, captures, metadata, active)` where captures is a table keyed by capture index. Use this when you need all captures for a pattern together.

**Parameters:**

- `{node}` ([`Node`](#maki-treesitter-Node)) Root node to search within.
- `{source}` (`string`) Source text the tree was parsed from.
- `{start_row?}` (`integer`) Only match rows >= this value (0-based).
- `{stop_row?}` (`integer`) Only match rows < this value (0-based).

**Returns:** (`function`) Iterator yielding (integer, table, table, integer).

**Example:**

```lua
local q = maki.treesitter.query.parse("lua", "(function_declaration name: (identifier) @name)"
)
for pat, captures, meta in q:iter_matches(root, source) do
  for cap_idx, nodes in pairs(captures) do
    print(nodes[1]:type())
  end
end
```


## maki.treesitter.Tree {#maki-treesitter-Tree}

A parsed syntax tree.

Obtained from `LanguageTree:parse()` or `LanguageTree:trees()`.
Call `:root()` to get the root node and start traversing.

```lua
local trees = parser:parse()
local root = trees[1]:root()
```

---

### `Tree:root()` {#Tree-root}

```lua
Tree:root()
```

Returns the root node of this tree. This is where you start walking
the syntax tree or running queries.

**Returns:** ([`Node`](#maki-treesitter-Node)) Root node.

**Example:**

```lua
local root = tree:root()
print(root:type()) -- e.g. "chunk" for Lua
```

---

### `Tree:copy()` {#Tree-copy}

```lua
Tree:copy()
```

Returns an independent copy of this tree.
Edits to the copy will not affect the original.

**Returns:** ([`Tree`](#maki-treesitter-Tree)) A new Tree with the same content.


## maki.treesitter.Node {#maki-treesitter-Node}

A single node in a parsed syntax tree.

Nodes are obtained from `Tree:root()`, navigation methods like `:child()`,
or from query captures. Each node knows its type, range, and children.

```lua
local root = tree:root()
print(root:type(), root:child_count())
for child, field in root:iter_children() do
  print(child:type(), field)
end
```

---

### `Node:type()` {#Node-type}

```lua
Node:type()
```

Returns the grammar type name for this node, like `"function_definition"` or `"identifier"`.

**Returns:** (`string`) Grammar type name.

---

### `Node:symbol()` {#Node-symbol}

```lua
Node:symbol()
```

Returns the numeric symbol id for this node's grammar type.
Two nodes with the same type always share the same symbol id.

**Returns:** (`integer`) Symbol id.

---

### `Node:id()` {#Node-id}

```lua
Node:id()
```

Returns a unique string identifier for this specific node in the tree.
Useful for deduplication or as a table key.

**Returns:** (`string`) Node identity string.

---

### `Node:range()` {#Node-range}

```lua
Node:range({include_bytes?})
```

Returns the range of this node as multiple return values.
Without {include_bytes}: `start_row, start_col, end_row, end_col`.
With {include_bytes} set to true: `start_row, start_col, start_byte, end_row, end_col, end_byte`.

**Parameters:**

- `{include_bytes?}` (`boolean`) When true, byte offsets are included in the return values.

**Returns:** (`integer`, `integer`, `integer`, `integer`) Four values, or six when include_bytes is true.

**Example:**

```lua
local sr, sc, er, ec = node:range()
local sr, sc, sb, er, ec, eb = node:range(true)
```

---

### `Node:start()` {#Node-start}

```lua
Node:start()
```

Returns the start position of this node: row, column, and byte offset (all 0-based).

**Returns:** (`integer`, `integer`, `integer`) start_row, start_col, start_byte.

---

### `Node:end_()` {#Node-end_}

```lua
Node:end_()
```

Returns the end position of this node: row, column, and byte offset (all 0-based).

**Returns:** (`integer`, `integer`, `integer`) end_row, end_col, end_byte.

---

### `Node:byte_length()` {#Node-byte_length}

```lua
Node:byte_length()
```

Returns how many bytes this node spans in the source text.

**Returns:** (`integer`) Byte length.

---

### `Node:child()` {#Node-child}

```lua
Node:child({index})
```

Returns the child at position {index} (0-based), including anonymous nodes like punctuation.
Returns nil if {index} is out of bounds.

**Parameters:**

- `{index}` (`integer`) 0-based child index.

**Returns:** ([`Node|nil`](#maki-treesitter-Node)) Child node, or nil.

---

### `Node:named_child()` {#Node-named_child}

```lua
Node:named_child({index})
```

Returns the named child at position {index} (0-based), skipping anonymous nodes.
Returns nil if {index} is out of bounds.

**Parameters:**

- `{index}` (`integer`) 0-based named child index.

**Returns:** ([`Node|nil`](#maki-treesitter-Node)) Named child node, or nil.

---

### `Node:child_count()` {#Node-child_count}

```lua
Node:child_count()
```

Returns the total number of children, including anonymous nodes.

**Returns:** (`integer`) Child count.

---

### `Node:named_child_count()` {#Node-named_child_count}

```lua
Node:named_child_count()
```

Returns the number of named children (skipping anonymous punctuation nodes).

**Returns:** (`integer`) Named child count.

---

### `Node:children()` {#Node-children}

```lua
Node:children()
```

Returns all children (named and anonymous) as a Lua table.

**Returns:** (`table`) Array of Node.

**Example:**

```lua
for _, child in ipairs(node:children()) do
  print(child:type())
end
```

---

### `Node:named_children()` {#Node-named_children}

```lua
Node:named_children()
```

Returns all named children as a Lua table, skipping anonymous nodes.

**Returns:** (`table`) Array of Node.

---

### `Node:iter_children()` {#Node-iter_children}

```lua
Node:iter_children()
```

Returns an iterator function that yields `(child, field_name)` for every child.
The field name is nil for children that are not assigned to a grammar field.

**Returns:** (`function`) Iterator yielding (Node, string|nil).

**Example:**

```lua
for child, field in node:iter_children() do
  if field then print(field .. ": " .. child:type()) end
end
```

---

### `Node:field()` {#Node-field}

```lua
Node:field({name})
```

Returns all children assigned to the grammar field {name} as a table.
For example, a function node might have a `"name"` or `"body"` field.

**Parameters:**

- `{name}` (`string`) Field name defined in the grammar.

**Returns:** (`table`) Array of Node.

**Example:**

```lua
local bodies = node:field("body")
```

---

### `Node:parent()` {#Node-parent}

```lua
Node:parent()
```

Returns the parent of this node, or nil if this is the root.

**Returns:** ([`Node|nil`](#maki-treesitter-Node)) Parent node.

---

### `Node:next_sibling()` {#Node-next_sibling}

```lua
Node:next_sibling()
```

Returns the next sibling (named or anonymous), or nil if this is the last child.

**Returns:** ([`Node|nil`](#maki-treesitter-Node)) Next sibling.

---

### `Node:prev_sibling()` {#Node-prev_sibling}

```lua
Node:prev_sibling()
```

Returns the previous sibling (named or anonymous), or nil if this is the first child.

**Returns:** ([`Node|nil`](#maki-treesitter-Node)) Previous sibling.

---

### `Node:next_named_sibling()` {#Node-next_named_sibling}

```lua
Node:next_named_sibling()
```

Returns the next named sibling, skipping anonymous nodes. Returns nil at the end.

**Returns:** ([`Node|nil`](#maki-treesitter-Node)) Next named sibling.

---

### `Node:prev_named_sibling()` {#Node-prev_named_sibling}

```lua
Node:prev_named_sibling()
```

Returns the previous named sibling, skipping anonymous nodes. Returns nil at the start.

**Returns:** ([`Node|nil`](#maki-treesitter-Node)) Previous named sibling.

---

### `Node:child_with_descendant()` {#Node-child_with_descendant}

```lua
Node:child_with_descendant({descendant})
```

Finds the direct child of this node that contains {descendant}.
Returns nil if {descendant} is not actually inside this node.

**Parameters:**

- `{descendant}` ([`Node`](#maki-treesitter-Node)) A node that may be a descendant.

**Returns:** ([`Node|nil`](#maki-treesitter-Node)) Direct child containing the descendant.

---

### `Node:descendant_for_range()` {#Node-descendant_for_range}

```lua
Node:descendant_for_range({start_row}, {start_col}, {end_row}, {end_col})
```

Finds the smallest node inside this node that spans the given point range.
Includes both named and anonymous nodes.

**Parameters:**

- `{start_row}` (`integer`) Start row (0-based).
- `{start_col}` (`integer`) Start column (0-based).
- `{end_row}` (`integer`) End row (0-based).
- `{end_col}` (`integer`) End column (0-based).

**Returns:** ([`Node|nil`](#maki-treesitter-Node)) Smallest node covering the range, or nil.

---

### `Node:named_descendant_for_range()` {#Node-named_descendant_for_range}

```lua
Node:named_descendant_for_range({start_row}, {start_col}, {end_row}, {end_col})
```

Like `descendant_for_range`, but only considers named nodes.

**Parameters:**

- `{start_row}` (`integer`) Start row (0-based).
- `{start_col}` (`integer`) Start column (0-based).
- `{end_row}` (`integer`) End row (0-based).
- `{end_col}` (`integer`) End column (0-based).

**Returns:** ([`Node|nil`](#maki-treesitter-Node)) Smallest named node covering the range, or nil.

---

### `Node:named()` {#Node-named}

```lua
Node:named()
```

Returns true if this is a named node (not anonymous punctuation like `,` or `(`).

**Returns:** (`boolean`)

---

### `Node:extra()` {#Node-extra}

```lua
Node:extra()
```

Returns true if this node is an "extra" (like a comment) that can appear anywhere in the grammar.

**Returns:** (`boolean`)

---

### `Node:missing()` {#Node-missing}

```lua
Node:missing()
```

Returns true if this node is "missing", meaning it was inserted by the parser during error recovery.

**Returns:** (`boolean`)

---

### `Node:has_error()` {#Node-has_error}

```lua
Node:has_error()
```

Returns true if this node or any of its descendants contain a syntax error.

**Returns:** (`boolean`)

---

### `Node:has_changes()` {#Node-has_changes}

```lua
Node:has_changes()
```

Returns true if this node has been marked as changed since the last parse.

**Returns:** (`boolean`)

---

### `Node:equal()` {#Node-equal}

```lua
Node:equal({other})
```

Returns true if this node and {other} are the same node in the tree.

**Parameters:**

- `{other}` ([`Node`](#maki-treesitter-Node)) Node to compare against.

**Returns:** (`boolean`)

---

### `Node:sexpr()` {#Node-sexpr}

```lua
Node:sexpr()
```

Returns the S-expression (lisp-like) string for this node and its children.
Handy for debugging the tree structure.

**Returns:** (`string`) S-expression.

**Example:**

```lua
print(node:sexpr()) -- e.g. "(identifier)"
```

---

### `Node:tree()` {#Node-tree}

```lua
Node:tree()
```

Returns the Tree that this node belongs to.

**Returns:** ([`Tree`](#maki-treesitter-Tree)) The owning tree.


## maki.treesitter.LanguageTree {#maki-treesitter-LanguageTree}

Manages parsing of a source string for a single language.

Obtained from `maki.treesitter.get_parser()` or `maki.treesitter.get_string_parser()`.
Call `:parse()` to get the syntax tree, then use `:root()` on the tree to start walking nodes.

```lua
local parser, err = maki.treesitter.get_parser(source, "lua")
if not err then
  local trees = parser:parse()
  local root = trees[1]:root()
end
```

---

### `LanguageTree:parse()` {#LanguageTree-parse}

```lua
LanguageTree:parse({range?})
```

Parses the source and returns a table containing the resulting Tree.
The tree is cached, so calling this again is cheap.

**Parameters:**

- `{range?}` (`table`) Unused. Accepted for API compatibility.

**Returns:** (`table`) Array with one Tree element.

**Example:**

```lua
local trees = parser:parse()
local root = trees[1]:root()
```

---

### `LanguageTree:lang()` {#LanguageTree-lang}

```lua
LanguageTree:lang()
```

Returns the language name this parser was created with.

**Returns:** (`string`) Language name, e.g. `"lua"`.

---

### `LanguageTree:children()` {#LanguageTree-children}

```lua
LanguageTree:children()
```

Returns child LanguageTrees for injected languages.
Not yet implemented, always returns an empty table.

**Returns:** (`table`) Empty table.

---

### `LanguageTree:trees()` {#LanguageTree-trees}

```lua
LanguageTree:trees()
```

Returns all parsed trees as a table (at most one for now).
Returns an empty table if `parse()` has not been called yet.

**Returns:** (`table`) Array of Tree.

---

### `LanguageTree:source()` {#LanguageTree-source}

```lua
LanguageTree:source()
```

Returns the source string this parser was created with.

**Returns:** (`string`) The original source text.

---

### `LanguageTree:is_valid()` {#LanguageTree-is_valid}

```lua
LanguageTree:is_valid({exclude_children?}, {range?})
```

Checks whether the parse tree is still valid.
Not yet implemented, always returns true.

**Parameters:**

- `{exclude_children?}` (`boolean`) Unused.
- `{range?}` (`table`) Unused.

**Returns:** (`boolean`) Always true.

---

### `LanguageTree:for_each_tree()` {#LanguageTree-for_each_tree}

```lua
LanguageTree:for_each_tree({fn})
```

Calls {fn} with `(tree, nil)` for the parsed tree.
Triggers a parse if the tree has not been parsed yet.

**Parameters:**

- `{fn}` (`function`) Callback receiving `(Tree, nil)`.

**Example:**

```lua
parser:for_each_tree(function(tree, _)
  print(tree:root():type())
end)
```

---

### `LanguageTree:included_regions()` {#LanguageTree-included_regions}

```lua
LanguageTree:included_regions()
```

Returns the regions this parser covers.
Not yet implemented, always returns a table with one empty region.

**Returns:** (`table`) Array with one empty table.

---

### `LanguageTree:contains()` {#LanguageTree-contains}

```lua
LanguageTree:contains({range})
```

Checks whether this parser covers the given {range}.
Not yet implemented, always returns true.

**Parameters:**

- `{range}` (`table`) Range to check (currently unused).

**Returns:** (`boolean`) Always true.

---

### `LanguageTree:destroy()` {#LanguageTree-destroy}

```lua
LanguageTree:destroy()
```

Drops the cached parse tree and frees its memory.
After calling this, the next `parse()` will re-parse from scratch.


## maki.ui {#maki-ui}

Functions for building interactive UI. Create buffers to hold
content, open floating or split windows to display them, highlight
code, render markdown, and show status hints.

```lua
local buf = maki.ui.buf()
buf:line("hello from my plugin!")
local win = maki.ui.open_win(buf, { title = "Greeting", width = "50%", height = 5 })
```

Without a UI (`maki -p`, the sdk, ACP), buffers and the text helpers
still work. The calls that need a screen behave like this:

- `input`, `input_edit`, `input_decorate`, `input_submit`, and `action`
  on a built-in name return `nil, "no interactive UI attached"`.
  Plugin actions still run.
- `open_editor` returns -1.
- `flash` writes to the log.
- `open_win`, `set_status_hint`, `set_window_title`, `set_mode` and `input_rules` have no effect.
  `input_rules` still throws on a bad rule, so a plugin's tests catch it.

### Chat input

Plugins can read, style, change and send the chat input:

| Job | Call |
| --- | --- |
| Read the text, the cursor and the selection | [`input`](#maki-ui-input) |
| Style text by what it says, like a `$skill:go` chip | [`input_rules`](#maki-ui-input_rules) |
| Style text from plugin state, like ghost text or diagnostics | [`input_decorate`](#maki-ui-input_decorate) |
| Change the text, the cursor or the selection | [`input_edit`](#maki-ui-input_edit) |
| Send the input, as Enter does | [`input_submit`](#maki-ui-input_submit) |
| React when the input changes | the `InputChanged` [event](#maki-api-create_autocmd) |
| Switch to normal mode for a vim mode | [`set_mode`](#maki-ui-set_mode), see [Modes](/docs/lua-api/#modes) |

A key that types `TODO: ` at the cursor:

```lua
maki.keymap.set("i", "<M-t>", function(ev)
  local st = ev.input
  maki.ui.input_edit({
    start = st.cursor, stop = st.cursor, text = "TODO: ",
    version = st.version, session_id = st.session_id,
  })
end)
```

When the look follows from the text, prefer `input_rules`. A rule runs
no Lua per keystroke, and its marks are right again after undo, redo,
a history recall or a chat switch.

The calls other than `input_rules` wait for the UI to answer. They work
from any callback, like a key handler or an autocmd. While your plugin
loads, they return `nil` and an error.

Moving the cursor or the selection needs the `input`
[permission](#plugin-permissions), because your next key lands at the
cursor. Changing the text needs `agent` and `session_write` as well,
what sending a message needs, because your next Enter sends it. This
covers `input_edit` and the built-in [actions](#maki-ui-action) that
edit, like `undo` or a history recall. `input_edit` needs
`session_read` too. Without them they raise `permission denied`.

Reading the input needs `session_read`. This covers `maki.ui.input`,
`ev.input` and `ev.text` in a key handler, and a binding on a key that
types text, see
[`maki.keymap.set`](#maki-keymap-set). Styling the input needs no
permission.

A plugin that may change the text may write any text. Before Enter
runs or sends it, it checks each plugin that wrote it:

| Text starts with | Each plugin that wrote it needs |
| --- | --- |
| `!` | `run` |
| a `/` command | what that command needs from a plugin, see [`maki.api.run_command`](#maki-api-run_command). Only the user can run some commands, like `/yolo` |
| anything else | `agent` and `session_write`, like [`input_submit`](#maki-ui-input_submit) |

If one of them falls short, Enter leaves the text where it is and the
status bar names that plugin. The text becomes the user's again once
it is sent or cleared, once another text replaces it, like a history
recall, or once the user replaces all of it, like selecting everything
and typing.

A draft a plugin wrote, restored after a restart, counts as written by
an unnamed plugin that holds `agent` and `session_write` at most,
which the status bar calls `a plugin`. So does a rewound or forked
turn that starts with `/` or `!`, or is `exit`.

Upgrading: `input_edit` needs `input` and `session_read` now, plus
`agent` and `session_write` to change the text, and the editing
built-in actions need `input`, `agent` and `session_write`. A binding
on a key that types text, like `a` or `<Paste>`, needs `session_read`
in any mode. A `plugin.toml` you wrote yourself grants them unless it
sets them to `false`. A package needs its author to add them, and you to approve
the package again.

### Offsets and versions

Offsets count bytes from 0, and a range leaves out `stop`. So the
range `start, stop` is `text:sub(start + 1, stop)`, and
`text:sub(1, cursor)` is everything before the cursor. A newline is
one byte.

A tab is one open session, named by its `session_id`. Its chats, the
main agent's and each subagent's, share one chat input, and each chat
keeps its own draft.

The calls work on the input of the focused tab. `input`, the
`InputChanged` event and `ev.input` of a key handler report its
`version` and `session_id`. Pass both to `input_edit`,
`input_decorate` and `input_submit`. Each tab counts versions from
zero, so a version alone cannot tell two tabs apart.

The user may type between your read and your call. `input_edit` and
`input_decorate` then move your offsets past what was typed, and
`input_submit` fails. All three fail when:

- `session_id` names a tab that is not focused.
- The text was replaced since: a history recall, a chat switch that
  swapped in another draft, a change made in `$EDITOR`, or a send.
- So much changed since `version` that the offsets can no longer be
  moved.

`input_edit` and `input_submit` also fail while the chat input is
hidden, since the user could not read what gets sent. It is hidden
while a permission prompt, the plan form or a `below` split takes its
place, or while a picker, a modal or a focused plugin window covers
it. `input_decorate` still works then.

A failed call returns `nil` and an error message. Read the input again
and retry. Bad arguments, like a missing or an unknown field, throw.

### Input marks

A mark styles a byte range of the input. `input_rules` puts one on
every match, and `input_decorate` and `input_edit` push marks at the
offsets you give. Marks and rules live in namespaces, named by `ns`.
Each plugin has its own namespaces, so two plugins can use the same
name. A mark has these fields:

| Field | Type | Effect |
| --- | --- | --- |
| `start`, `stop` | integer | The range, for pushed marks only. Without `stop` the mark is a point, which only `virt_text` shows. |
| `hl` | string or table | A theme style name like `"accent"`, or `{ fg, bg, bold, italic, underline, dim, strikethrough, reversed }`. |
| `conceal` | string | Text drawn in place of the range. `""` hides it. |
| `reveal` | string | `"cursor"` shows the raw text while the cursor is inside the range or on its edge. |
| `atomic` | boolean | The cursor steps over the range, a selection takes it whole, and a delete that cuts into it removes all of it. |
| `virt_text` | table | Spans drawn but never part of the text, `{ { text, hl? }, ... }`. |
| `virt_text_pos` | string | `"eol"` (default) draws `virt_text` after the line, `"inline"` before `start`. |
| `lang` | string | Highlights the range as code in this language. |

The text of `conceal` and `virt_text` must not hold control
characters, newlines included. A mark never edits the text, so a send
carries the raw text, concealed ranges included.

Where marks overlap:

- A pushed mark draws over every rule, and a mark pushed later over
  one pushed earlier.
- A rule whose `ns` was first set later draws over one set earlier.
  Replacing a rule keeps its place.
- Within a tree-sitter rule, a later pattern draws over an earlier
  one, and `markdown_inline` over `markdown`.
- Within one pattern, a capture whose name first appears later in the
  query draws over one that appears earlier. In
  `(inline_link (link_text) @text) @link`, `@link` draws over `@text`.

Pushed marks move with the text: a range keeps covering the same
words, and a point stays after text typed at it. A range is dropped
once its text is gone, or when an edit lands inside it while it
conceals or is atomic. Undo brings back the text without its marks, so
undoing the delete of a chip shows its raw text. Marks stay with their
draft across chat switches, go away when the plugin unloads, and are
not saved.

---

### `maki.ui.buf()` {#maki-ui-buf}

```lua
maki.ui.buf({opts?})
```

Creates a new buffer for building UI content. The first buffer
created in a task becomes the "live" buffer, streamed to the UI while
the tool runs, which is what the tool's own output pane wants. A
float that opens during a tool call would take that spot away, so
create its buffer with `{ scratch = true }`. It matches nvim's
`nvim_create_buf(false, true)`. A buffer belongs to the task that
made it, so this throws at a plugin's top level.

**Parameters:**

- `{opts?}` (`table?`) Optional. `scratch` (boolean) keeps the buffer out of the live slot, default false.

**Returns:** ([`Buf`](#maki-ui-Buf)) Buffer handle.

**Example:**

```lua
-- The tool's output pane:
local out = maki.ui.buf()
out:line("hello world")

-- A float raised during a tool call needs its own buffer:
local toast = maki.ui.buf({ scratch = true })
toast:line("copied!")
```

---

### `maki.ui.theme_color()` {#maki-ui-theme_color}

```lua
maki.ui.theme_color({name})
```

Looks up a color the syntax theme names, such as "background",
"foreground" or "accent". For the styles the UI paints with, use
`maki.ui.theme_style`.

**Parameters:**

- `{name}` (`string`) Syntax theme color name, e.g. "accent" or "background".

**Returns:** (`string|nil`) "#rrggbb" for a truecolor theme, a palette index as a
  string like "4" when the theme names an ANSI color, or "default" for the
  terminal's own color. Nil only when the name is unknown. Every form can be
  passed straight to a span's `fg`/`bg`.

**Example:**

```lua
local accent = maki.ui.theme_color("accent")
if accent then
  buf:line({ { "note", { fg = accent, bold = true } } })
end
```

---

### `maki.ui.theme_style()` {#maki-ui-theme_style}

```lua
maki.ui.theme_style({name})
```

Looks up a named style from the current theme. The names are the ones a span
already takes as a string ("dim", "item_selected", "keybind_section",
"diff_old", ...), so `{ text, "dim" }` and `theme_style("dim")` paint the
same. Reach for the table when you need the parts, say to keep a style's
foreground over a background of your own.

**Parameters:**

- `{name}` (`string`) Style name, the same spelling a span accepts.

**Returns:** (`table|nil`) `{fg?, bg?, bold?, italic?, underline?, dim?,
  strikethrough?, reversed?}`, ready to use as a span style. Colors are
  spelled as in `maki.ui.theme_color`. Nil when the name is unknown, and an
  empty table when the theme leaves that style unset.

**Example:**

```lua
local sel = maki.ui.theme_style("item_selected")
local dim = maki.ui.theme_style("dim")
buf:line({ { "note", { fg = dim.fg, bg = sel.bg } } })
```

---

### `maki.ui.highlight()` {#maki-ui-highlight}

```lua
maki.ui.highlight({code}, {lang}, {opts?})
```

Syntax-highlights a chunk of source code. Returns a table of styled
lines that you can feed into a buffer. Each line is a list of
`{text, style}` spans where style is a `{fg, bold?, italic?, underline?}` table.

`fg` is "#rrggbb" for a truecolor theme, a palette index as a string like
"4" when the theme names an ANSI color, or "default" for the terminal's own
color. Pass the span straight to `buf:line` and it resolves correctly in
every case.

**Parameters:**

- `{code}` (`string`) Source text to highlight.
- `{lang}` (`string`) Language identifier, e.g. "rust", "python".
- `{opts?}` (`table?`) Options. Fields:
  - `independent` (`boolean`) highlight each line without cross-line context. Default false.
  - `prefix` (`string`) preceding source used as token context. Only the nearest lines fitting within 16 KiB are parsed, so earlier comments or strings may affect coloring. Default "".

**Returns:** (`table`) Lines: `{ { {text, style}, ... }, ... }`. Each style is `{fg, bold?, italic?, underline?}`.

**Example:**

```lua
local lines = maki.ui.highlight("fn main() {}", "rust")
for _, spans in ipairs(lines) do
  buf:line(spans)
end
```

---

### `maki.ui.markdown()` {#maki-ui-markdown}

```lua
maki.ui.markdown({text}, {width})
```

Renders Markdown into styled lines ready to display in a buffer.
Each span's style is either a named string ("bold", "heading",
"inline_code", etc.) or a `{fg, bold?, italic?, underline?}` table
for syntax-highlighted code blocks.

**Parameters:**

- `{text}` (`string`) Markdown source.
- `{width}` (`integer`) Wrap width in columns.

**Returns:** (`table`) Lines: `{ { {text, style}, ... }, ... }`.

**Example:**

```lua
local size = maki.ui.terminal_size()
local lines = maki.ui.markdown("# Hello\n\nSome **bold** text.", size.cols)
for _, spans in ipairs(lines) do
  buf:line(spans)
end
```

---

### `maki.ui.humantime()` {#maki-ui-humantime}

```lua
maki.ui.humantime({secs})
```

Formats a number of seconds into a short, human-friendly string.
Useful for displaying elapsed time in status messages.

**Parameters:**

- `{secs}` (`integer`) Duration in seconds.

**Returns:** (`string`) Human-readable duration, e.g. "1m30s".

**Example:**

```lua
maki.ui.humantime(90)   -- "1m30s"
maki.ui.humantime(3661) -- "1h1m1s"
```

---

### `maki.ui.terminal_size()` {#maki-ui-terminal_size}

```lua
maki.ui.terminal_size()
```

Returns the current terminal size. Handy for sizing floating windows
or wrapping text to fit the screen.

**Returns:** (`table`) `{cols, rows}`, terminal width and height in characters.

**Example:**

```lua
local size = maki.ui.terminal_size()
local half_width = math.floor(size.cols / 2)
```

---

### `maki.ui.display_width()` {#maki-ui-display_width}

```lua
maki.ui.display_width({text})
```

Returns the display width of a string in terminal cells, matching
how `ratatui` measures text.

**Parameters:**

- `{text}` (`string`) The text to measure.

**Returns:** (`integer`) Number of display cells the text occupies.

**Example:**

```lua
local w = maki.ui.display_width("hello")
```

---

### `maki.ui.truncate_text()` {#maki-ui-truncate_text}

```lua
maki.ui.truncate_text({text}, {max_width})
```

Splits a string at a display-cell boundary.

**Parameters:**

- `{text}` (`string`) The text to split.
- `{max_width}` (`integer`) Maximum display cells for the head.

**Returns:** (`table`) `{head = string, tail = string}`.

**Example:**

```lua
local t = maki.ui.truncate_text("hello world", 5)
-- t.head == "hello", t.tail == " world"
```

---

### `maki.ui.set_mode()` {#maki-ui-set_mode}

```lua
maki.ui.set_mode({mode})
```

Switches the chat input to normal mode (`"n"`) or insert mode (`"i"`),
like leaving and entering insert mode in Neovim.

The mode is global and stays until a plugin switches it again, apart from
the times [Modes](/docs/lua-api/#modes) lists. It applies at once:
`maki.fn.mode()` returns it and the next key is matched against it. If
your plugin unloads while the input is in normal mode, maki switches it
back to insert mode.

Throws on any mode but `"n"` and `"i"`.

**Parameters:**

- `{mode}` (`string`) `"n"` or `"i"`.

**Example:**

```lua
maki.keymap.set("n", "i", function() maki.ui.set_mode("i") end)
```

---

### `maki.ui.flash()` {#maki-ui-flash}

```lua
maki.ui.flash({msg})
```

Shows a short-lived message in the status bar, such as "copied!" or a
transient warning. Without a UI, the message goes to the log.

**Parameters:**

- `{msg}` (`string`) Message text.

**Example:**

```lua
maki.ui.flash("Copied to clipboard!")
```

---

### `maki.ui.action()` {#maki-ui-action}

```lua
maki.ui.action({name})
```

Runs a built-in or plugin action by name, and returns whether it handled
the request.

A built-in does what its default key does, so you can move it to another
key:

| Built-in | What it does | `false` when |
| --- | --- | --- |
| `"file_picker"` | Opens the file picker | |
| `"search"` | Opens message search | |
| `"help"` | Opens or closes `/help` | |
| `"model_picker"` | Opens the model picker | |
| `"plan_toggle"` | Opens or closes the plan form | no plan is ready |
| `"plan_editor"` | Opens the plan in your editor | there is no plan |
| `"edit_input"` | Opens the chat input in your editor | |
| `"pop_queue"` | Removes the first queued message | |
| `"prev_chat"`, `"next_chat"` | Shows the previous or next chat, with its own draft | |
| `"input_raw"` | Switches the input between drawing what plugins conceal and showing the text under it, which is what gets sent | |
| `"undo"`, `"redo"` | Undoes or redoes one input edit | nothing to undo or redo |
| `"history_prev"`, `"history_next"` | Recalls the previous or next prompt | at either end of the history |
| `"newline"` | Inserts a new line | |

`"undo"`, `"redo"`, `"history_prev"`, `"history_next"` and `"newline"`
edit the chat input, also in normal mode. They need the `input`, `agent`
and `session_write` permissions, see [chat input](#chat-input), and throw
without them like any gated call. They return `false` in a finished subagent's chat, which has
no input. `"pop_queue"` needs `session_write`, because it throws away a
queued prompt. The other built-ins need no permission.

A built-in runs in the UI after your earlier `maki.ui` calls, and
`action` waits for its answer. While your plugin loads, it returns `nil`
and an error at once.

A plugin action has a dot in its name, like `"completion.accept"`. It
comes from [`maki.api.register_action`](#maki-api-register_action), which
also says what permissions a caller needs. It runs at once, also while
your plugin loads, and `action` returns what it returned.

`action` returns `nil` and an error, which an `if` treats as not handled,
when:

- The action's plugin is not loaded.
- The action threw.
- Your plugin lacks a permission a plugin action needs. The status bar
  shows this too.
- A built-in runs without a UI.

It throws on a name that cannot be right: no dot and not a built-in, or
one its loaded plugin never registered.

To try several actions on one key, see
[Composing keys](/docs/keybindings/#composing-keys). To send the input,
use `maki.ui.input_submit`. For slash commands, see `maki.api.run_command`.

**Parameters:**

- `{name}` (`string`) Action name, e.g. `"file_picker"` or `"completion.accept"`.

**Returns:** (`boolean|nil`, `string|nil`) Whether the action handled it, or nil and an error.

**Example:**

```lua
-- A vim plugin's undo in normal mode, which needs `input`, `agent` and `session_write`:
maki.keymap.set("n", "u", function() maki.ui.action("undo") end)

-- A key for a built-in that has none:
maki.keymap.set({ "n", "i" }, "<M-r>", function() maki.ui.action("input_raw") end)
```

---

### `maki.ui.open_editor()` {#maki-ui-open_editor}

```lua
maki.ui.open_editor({path})
```

Opens {path} in the user's `$EDITOR` (e.g. vim, nano) and suspends the
TUI until the editor exits. An exit code of 0 does not mean the user
saved: read the file back to see what changed.

**Parameters:**

- `{path}` (`string`) File to open.

**Returns:** (`integer`) Editor exit code, or -1 if the editor failed to start or there is no UI.

**Example:**

```lua
local code = maki.ui.open_editor("/tmp/scratch.lua")
if code ~= 0 then
  maki.ui.flash("editor exited with " .. code)
end
```

---

### `maki.ui.open_win()` {#maki-ui-open_win}

```lua
maki.ui.open_win({buf}, {opts})
```

Opens a floating or split window that displays the contents of {buf}.
Returns a Win handle you can use to receive events, update layout,
and close the window when you are done.

A window belongs to one session and shows only while that session's tab is
focused. It closes when the session leaves its tab (`/new`, loading another
session, closing the tab), or right away if the session is not in any tab.

**Parameters:**

- `{buf}` ([`Buf`](#maki-ui-Buf)) Buffer to display.
- `{opts}` (`table`) Float configuration. Fields:
  - `width` (`integer|string`) window width. Integer for absolute columns; "N%" for percent of terminal width. Default "60%".
  - `height` (`integer|string`) window height. Integer for absolute rows; "N%" for percent of terminal height. Default "70%".
  - `row` (`integer?`) row offset from the anchor corner. Negative values move up.
  - `col` (`integer?`) column offset from the anchor corner.
  - `anchor` (`string`) corner the (row, col) offset is relative to. One of "NW" (default), "NE", "SW", "SE". Or "input_caret", which sits the window beside the chat input cursor: the host takes the roomier side of the cursor, trims the height to what fits there, keeps the whole width on screen, and re-places it on every redraw, so it follows wraps, resizes and any modal taking focus. `row` and `col` shift the window off that spot, and `stack` grows the next one away from the cursor. With no cursor on screen, because a form, a permission prompt or a `below` split has taken the input box, it falls back to the centred default, `row` and `col` still applying.
  - `border` (`string`) border style. One of "rounded" (default), "single", "double", "none".
  - `title` (`string`) text shown in the top border. Default "".
  - `title_pos` (`string`) title alignment. One of "left" (default), "center", "right".
  - `footer` (`table`) key-hint pairs shown in the bottom border. Each entry is {key, label}. A bordered float is widened to fit its title and footer, up to the screen width.
  - `zindex` (`integer`) stacking order. Default 50.
  - `cursor_line` (`boolean`) highlight the focused row. Default false.
  - `reserved_top` (`integer`) rows reserved at the top of the content area. Default 0.
  - `reserved_bottom` (`integer`) rows reserved at the bottom of the content area. Default 0.
  - `split` (`string`) dock the window to an edge instead of floating. One of "above", "below", "left", "right", "panel", or "" (floating, default).
  - `order` (`integer`) paint order among split windows at the same edge. Default 50.
  - `focus` (`boolean`) whether the window takes keyboard focus on open. Default true.
  - `keys` (`table`) keys this window takes while it is on screen, in `maki.keymap` notation, e.g. `{ "<Tab>", "<CR>" }`. `maki.keymap.set` with `{ buffer = buf }` is the newer way to give a popup keys, with a handler per key, a mode, and `ev.fallback()`. Requires `focus = false`, since a focused window already gets every key. A claimed key goes to this window's `recv` and never reaches the chat input or global `maki.keymap.set` bindings. The window's buffer bindings are asked first. Claims are released automatically when the window closes, and a hidden or zero-size window claims nothing. Host pickers and the slash command palette take keys first while open over the window. `<C-c>` and `<C-z>` are refused.
  - `visible` (`boolean`) whether the window is initially visible. Default true. See `win:hide()` for what hiding does.
  - `needs_input` (`boolean`) whether the window means the session needs user input. Default false.
  - `stack` (`boolean`) offset the window past the other stacked windows sharing its anchor, in open order, with a one row gap. Closing one moves the rest up. Floating windows only. Default false.
  - `session` (`string`) id of the session that owns the window, e.g. `ev.data.session_id` or `ctx:session_id()`. Default: the session of the running tool call, else the focused tab's. A `maki.async.run` task keeps the tool call's session, a `maki.async.spawn` task does not.

**Returns:** ([`Win`](#maki-ui-Win)) Window handle.

**Example:**

```lua
local buf = maki.ui.buf()
buf:line("Pick an option:")
local win = maki.ui.open_win(buf, {
  title = "Menu",
  width = "50%",
  height = 10,
  cursor_line = true,
  footer = { { "q", "quit" }, { "Enter", "select" } },
})
```

---

### `maki.ui.set_status_hint()` {#maki-ui-set_status_hint}

```lua
maki.ui.set_status_hint({spans}, {opts?})
```

Shows your plugin's key hints in the status bar. Other plugins' hints are not affected. By default the hints show in every tab, even when set from a tool call. With `opts.session` they show only in that session's tab, replacing your every-tab hints there, and are cleared when the session leaves the tab.

**Parameters:**

- `{spans}` (`table|nil`) Sequence of {key, label} pairs, e.g. `{{"q", "quit"}, {"j", "down"}}`. nil clears your hints for `opts.session`, or all your hints if `opts.session` is not set.
- `{opts}` (`table?`) `session` (string): id of the session whose tab shows the hints, e.g. `ev.data.session_id` or `ctx:session_id()`. Default: every tab.

**Example:**

```lua
maki.ui.set_status_hint({ {"q", "quit"}, {"j", "down"} })
-- later, clear them:
maki.ui.set_status_hint(nil)

-- Only in the tab of the session that is running a turn:
maki.api.create_autocmd("TurnStart", {
  callback = function(ev)
    maki.ui.set_status_hint({ {"Esc", "cancel"} }, { session = ev.data.session_id })
  end,
})
maki.api.create_autocmd("TurnEnd", {
  callback = function(ev)
    maki.ui.set_status_hint(nil, { session = ev.data.session_id })
  end,
})
```

---

### `maki.ui.set_window_title()` {#maki-ui-set_window_title}

```lua
maki.ui.set_window_title({title})
```

Sets the terminal emulator's window title. Pass an empty string to
clear it.

The title passes through tmux, GNU screen, and zellij untouched, and
control characters are stripped, so model text cannot inject escape
sequences into the terminal. On exit maki hands the title back to the
shell, on terminals that support the title stack.

**Parameters:**

- `{title}` (`string`) New window title, e.g. `"● 3/5 tests"`.

**Example:**

```lua
maki.ui.set_window_title("maki: " .. session_name)
-- Give the title back to the shell:
maki.ui.set_window_title("")
```

---

### `maki.ui.input()` {#maki-ui-input}

```lua
maki.ui.input()
```

Reads the chat input of the focused tab.

The returned table has:

- `text` (string) the whole value, newlines included.
- `cursor` (integer) byte offset of the cursor.
- `anchor` (integer|nil) byte offset of the other end of the selection,
  nil when nothing is selected. The selected bytes run from
  `math.min(anchor, cursor)` to `math.max(anchor, cursor)`.
- `version` (integer) and `session_id` (string), for the calls that
  write. See [offsets and versions](#offsets-and-versions).

The table has no line or column. The example works them out. To open a
window at the cursor, pass `anchor = "input_caret"` to
[`open_win`](#maki-ui-open_win), an option unrelated to the selection
`anchor` here.

Requires the `session_read` [plugin permission](#plugin-permissions).

**Returns:** (`table|nil`, `string|nil`) The input state, or nil and an error without a UI.

**Example:**

```lua
local st = maki.ui.input()
local before = st.text:sub(1, st.cursor)
local line = select(2, before:gsub("\n", "")) -- 0-based
local col = #before:match("[^\n]*$") -- in bytes
```

---

### `maki.ui.input_rules()` {#maki-ui-input_rules}

```lua
maki.ui.input_rules({opts})
```

Sets a rule that marks the chat input in every tab, from a regex or a
tree-sitter query. See [chat input](#chat-input) for when to use one.

Each `ns` of your plugin holds one rule. Calling again with the same
`ns` replaces it, and a call with only `ns` removes it. Rules go away
when the plugin unloads. A bad pattern, query or mark throws. Where
marks overlap, [input marks](#input-marks) says which draws on top.

A regex rule marks every match of `pattern` with `mark`. The pattern
uses [Rust regex syntax](https://docs.rs/regex/latest/regex/#syntax).
Matching runs one line at a time, so a match never spans lines. The
pattern still sees the text before its line: `\A` is the start of the
input, `$` the end of the line and `(?m)^` the start of one. An empty
match marks a point. In `conceal` and the text of `virt_text`, `$1` is
capture group 1, `$0` the whole match and `$$` a plain `$`.

A tree-sitter rule parses the input as `lang` and runs `query` on it, as
the `treesitter` fields below describe. Parsing runs in the background,
and until it catches up with typing, its marks move with the text like
pushed marks.

**Parameters:**

- `{opts}` (`table`) Options:
  - `ns` (`string`) the namespace.
  - `pattern` (`string|nil`) the regex to match.
  - `mark` (`table|nil`) the [mark](#input-marks) each match gets, without `start` and `stop`. Required with `pattern`.
  - `treesitter` (`table|nil`) a tree-sitter rule, in place of `pattern` and `mark`.
  - `treesitter.lang` (`string`) the language the input is parsed as, like `"markdown"`.
  - `treesitter.query` (`string|table`) the query for `lang`, or a table of language = query that gives each language in the input its own. The predicates are those of `maki.treesitter.query`, and `#set!` only takes `injection.language`.
    - Code captured as `@injection.content` is in the language `@injection.language` caught, or the one `#set! injection.language` names. When the rule has a query for that language, the code is parsed again and that query runs there. Otherwise the code gets a `lang` mark.
    - A `markdown` rule runs its `markdown_inline` query inside blocks by itself.
  - `treesitter.captures` (`table|nil`) capture name = template, with the fields of `mark`. Templates are drawn as written, `$` included.
    - Each capture needs a template, and each template a capture.
    - `@injection.content` may go without one. `@injection.language` and names starting with `_` take none, since they only name the injected language or feed predicates.

**Example:**

```lua
-- Draw `$skill:go` as a ` go ` chip the cursor steps over:
maki.ui.input_rules({
  ns = "skill",
  pattern = [[\$skill:([\w-]+)]],
  mark = { conceal = " $1 ", hl = "accent", atomic = true },
})
-- And remove it again:
maki.ui.input_rules({ ns = "skill" })
-- Bold text without its stars, unless the cursor is on it:
maki.ui.input_rules({
  ns = "md",
  treesitter = {
    lang = "markdown",
    query = { markdown_inline = [[ (strong_emphasis (emphasis_delimiter) @hidden) @strong ]] },
    captures = {
      hidden = { conceal = "", reveal = "cursor" },
      strong = { hl = { bold = true } },
    },
  },
})
```

---

### `maki.ui.input_decorate()` {#maki-ui-input_decorate}

```lua
maki.ui.input_decorate({opts})
```

Replaces all the marks your plugin has in namespace `ns` of the chat
input. Use it for looks that come from plugin state. When the look follows
from the text, a [rule](#maki-ui-input_rules) fits better.

Keep a look that tracks something live, like ghost text at the cursor, in
one namespace and replace it on `InputChanged`. Marks placed once, like
the chip over a paste, each need a namespace of their own.

A mark that the typing since `version` would have
[dropped](#input-marks) is skipped, so typing alone never fails the call.

Besides the cases in [offsets and versions](#offsets-and-versions), the
call fails when a mark ends past the end of the text, an offset falls
inside a multi-byte character, or `start` is past `stop`.

**Parameters:**

- `{opts}` (`table`) Options:
  - `ns` (`string`) the namespace to replace.
  - `marks` (`table`) list of [marks](#input-marks), `{}` to clear the namespace.
  - `version` (`integer`) the version the offsets were planned against.
  - `session_id` (`string`) the tab the offsets were read from.

**Returns:** (`table|nil`, `string|nil`) `{ version }`, the version the marks were placed on, or nil and an error.

**Example:**

```lua
maki.api.create_autocmd("InputChanged", {
  callback = function(ev)
    maki.ui.input_decorate({
      ns = "ghost",
      marks = { { start = ev.data.cursor, virt_text = { { "complete this", "dim" } }, virt_text_pos = "inline" } },
      version = ev.data.version,
      session_id = ev.data.session_id,
    })
  end,
})
```

---

### `maki.ui.input_edit()` {#maki-ui-input_edit}

```lua
maki.ui.input_edit({opts})
```

Replaces byte ranges of the chat input, as if the user had selected each
one and typed its `text`. All edits land as one change, with one new
version and one undo step.

Plan every range against the text at `version`, without counting the
other edits in the list. Ranges must not overlap, and no two may start
at the same offset. With `edits = {}` the call only moves the cursor or
the selection, or replaces the marks in `ns`, so it needs `cursor`,
`anchor` or `marks`. It makes no new version and no undo step.

Besides the cases in [offsets and versions](#offsets-and-versions), an
edit fails when:

- The user typed at or next to one of its ranges since `version`. Two
  inserts at the same offset count, because no order is right for both.
- `cursor` or `anchor` lands inside text changed since `version`.
- `strict` is set and `version` is not current.
- A range ends past the end of the text, or an offset falls inside a
  multi-byte character.

Enter checks the text you write before it runs it, see
[chat input](#chat-input). For marks alone, use `input_decorate`, which
needs no permission.

`text` is cleaned the way a paste is: tabs become spaces, carriage
returns become newlines, and other control characters are dropped. With
`cursor`, `anchor` or `marks` given, text that needs cleaning fails
instead, since their offsets would no longer match. `ev.text` of
`<Paste>` is already clean.

It also needs `session_read`, like `maki.ui.input`. Its answer and its
errors tell where the user typed since `version`, so a plugin could
learn the input from them.

Requires the `input` and `session_read` [plugin permissions](#plugin-permissions).

**Parameters:**

- `{opts}` (`table`) Options:
  - `edits` (`table|nil`) list of `{ start, stop, text }` tables, `{}` for none. Without it, `start`, `stop` and `text` are read from {opts} as one edit.
  - `start` (`integer`) byte offset where the range starts.
  - `stop` (`integer`) byte offset where it ends. Equal to `start` to insert.
  - `text` (`string`) the new text, `""` to delete the range. Required.
  - `version` (`integer`) the version the offsets were planned against.
  - `session_id` (`string`) the tab the offsets were read from.
  - `cursor` (`integer|nil`) byte offset to leave the cursor at, counted in the text the edits produce, as if nobody else had typed. Default is the end of the new text of the edit with the highest `start`, or the cursor as it is when `edits` is empty.
  - `anchor` (`integer|nil`) byte offset to select from, up to `cursor`, counted like `cursor`. Without it nothing stays selected.
  - `strict` (`boolean|nil`) accept only the current version, instead of moving the edits past newer typing. Default false.
  - `marks` (`table|nil`) [marks](#input-marks) to replace namespace `ns` with, counted like `cursor`. They appear together with the text. A mark that does not fit fails the whole edit.
  - `ns` (`string|nil`) the namespace for `marks`. Required with `marks`.

**Returns:** (`table|nil`, `string|nil`) `{ version, cursor }` after the edit, or nil and an error. Pass this `version` to a call that follows.

**Example:**

```lua
-- Insert each paste under a chip, in a namespace of its own.
local pastes = 0
maki.keymap.set("i", "<Paste>", function(ev)
  local st = ev.input
  pastes = pastes + 1
  local done = maki.ui.input_edit({
    start = st.cursor, stop = st.cursor, text = ev.text,
    ns = "paste" .. pastes,
    marks = { { start = st.cursor, stop = st.cursor + #ev.text, conceal = "[pasted text]", atomic = true } },
    version = st.version, session_id = st.session_id,
  })
  if not done then ev.fallback() end
end)

-- Wrap the selection in bold and keep the same words selected:
local st = maki.ui.input()
if not st.anchor then return end
local s, e = math.min(st.anchor, st.cursor), math.max(st.anchor, st.cursor)
maki.ui.input_edit({
  edits = {
    { start = s, stop = s, text = "**" },
    { start = e, stop = e, text = "**" },
  },
  cursor = e + 2,
  anchor = s + 2,
  version = st.version,
  session_id = st.session_id,
})
```

---

### `maki.ui.input_submit()` {#maki-ui-input_submit}

```lua
maki.ui.input_submit({opts})
```

Sends the chat input the way Enter does, `/` commands, `!` lines, images,
the queue and the history included. Only a `\` before the cursor differs:
Enter turns it into a new line, and `input_submit` sends it as it is.

Use it to send what a key handler wrote, since `ev.fallback()` on `<CR>`
refuses text that changed after the press, see
[`maki.keymap.set`](/docs/lua-api/#maki-keymap-set).

It sends only while the input still holds `version`, so what goes out is
the text your plugin planned against. After your own edit, pass the
version `input_edit` returned. In a key handler, `ev.input.version` is
the version at the press. Any other change makes the version stale, like
an edit of another plugin or a key that landed after the one second keys
wait for a handler.

A send needs the [plugin permissions](/docs/lua-api/#plugin-permissions)
that doing the same thing directly needs:

- A message needs `agent` and `session_write`, like
  `maki.session.prompt`, also when it waits in the queue or is the retry
  Enter sends after a login.
- A `!` or `!!` line needs `run`.
- A `/` command needs what `maki.api.run_command` needs for it, so
  `/yolo`, `/trust`, `/packupdate` and `/packdel` always fail.

Besides the cases in [offsets and versions](#offsets-and-versions), the
call fails, and the input stays as it is, when:

- The message queue has the keyboard after `/queue`.
- There is nothing to send, or the command palette is open with no
  command to run.
- Your plugin lacks a permission listed above.
- A plugin changed the text, and Enter would refuse it for that plugin,
  see [chat input](#chat-input).

**Parameters:**

- `{opts}` (`table`) Options:
  - `version` (`integer`) the version of the text to send, from `maki.ui.input`, `ev.input` or `input_edit`.
  - `session_id` (`string`) the tab the version was read from.

**Returns:** (`boolean|nil`, `string|nil`) `true` once sent, or nil and an error.

**Example:**

```lua
-- Spell out `lgtm` before sending:
maki.keymap.set("i", "<CR>", function(ev)
  local st = ev.input
  local s, e = st.text:find("lgtm", 1, true)
  local version = st.version
  if s then
    local r = maki.ui.input_edit({
      start = s - 1, stop = e, text = "looks good to me",
      version = st.version, session_id = st.session_id,
    })
    if not r then return end
    version = r.version
  end
  maki.ui.input_submit({ version = version, session_id = st.session_id })
end)
```


## maki.ui.Win {#maki-ui-Win}

Handle to a floating or split window. You get one from
`maki.ui.open_win()`. Use `recv()` in a loop to handle keyboard
input, and call `close()` when done.

Fields: `width`, `height` (initial content dimensions in columns/rows),
`visible` (current visibility).

```lua
local win = maki.ui.open_win(buf, { title = "Demo" })
while true do
  local ev = win:recv()
  if not ev or ev.key == "q" then break end
end
win:close()
```

---

### `Win:recv()` {#Win-recv}

```lua
Win:recv({timeout_ms?})
```

Waits for the next event from this window. Call this in a loop to build an interactive UI. Returns nil once the window is closed or the channel disconnects. Pass {timeout_ms} to also get `{type="timeout"}` events so your plugin can animate while idle.

Event tables by type:
- `{type="key", key}` -- keypress. {key} is in canonical `maki.keymap` notation: `"q"`, `"<CR>"`, `"<Esc>"`, `"<C-n>"`, `"<S-Tab>"`.
- `{type="resize", width, height}` -- terminal was resized.
- `{type="paste", text}` -- bracketed paste.
- `{type="close"}` -- window was closed externally.
- `{type="timeout"}` -- no event arrived within {timeout_ms}.

**Parameters:**

- `{timeout_ms?}` (`integer`) Max milliseconds to wait before a timeout event is returned.

**Returns:** (`table|nil`) Event table, or nil if the window has closed.

**Example:**

```lua
while true do
  local ev = win:recv()
  if not ev or ev.key == "q" then break end
  if ev.type == "key" and ev.key == "<Down>" then
    -- move cursor down
  end
end
win:close()
```

---

### `Win:set_config()` {#Win-set_config}

```lua
Win:set_config({opts})
```

Updates the window layout on the fly. Only the fields you include in
{opts} are changed, everything else stays the same.

**Parameters:**

- `{opts}` (`table`) Partial float config. Accepted fields:
  - `title` (`string`) border title text.
  - `title_pos` (`string`) title alignment, "left", "center", or "right".
  - `footer` (`table`) key-hint pairs `{{key, label}, ...}` shown in the bottom border.
  - `border` (`string`) "rounded", "single", "double", or "none".
  - `anchor` (`string`) corner origin, "NW", "NE", "SW", "SE", or "input_caret".
  - `width` (`integer|string`) new width; integer or "N%".
  - `height` (`integer|string`) new height; integer or "N%".
  - `zindex` (`integer`) stacking order.
  - `cursor_line` (`boolean`) highlight the focused row.
  - `reserved_top` (`integer`) rows reserved at the top of the content area.
  - `split` (`string`) edge docking, "above", "below", "left", "right", "panel", or "".
  - `order` (`integer`) paint order among split windows.
  - `needs_input` (`boolean`) whether the window means the session needs user input.

**Example:**

```lua
win:set_config({ title = "Updated!", width = "80%" })
```

---

### `Win:set_cursor()` {#Win-set_cursor}

```lua
Win:set_cursor({row})
```

Moves the highlighted cursor line to {row} (1-indexed). Only has a
visible effect when the window was opened with `cursor_line = true`.

**Parameters:**

- `{row}` (`integer`) Target row, 1-indexed.

**Example:**

```lua
win:set_cursor(3) -- highlight the third line
```

---

### `Win:close()` {#Win-close}

```lua
Win:close()
```

Closes the window and frees its resources. Safe to call more than
once. The window also closes automatically when the handle is
garbage collected.

**Example:**

```lua
win:close()
```

---

### `Win:is_open()` {#Win-is_open}

```lua
Win:is_open()
```

Returns true if the window is still alive (not closed). Useful for
checking before sending commands.

**Returns:** (`boolean`) true if open.

**Example:**

```lua
if win:is_open() then
  win:set_config({ title = "still here" })
end
```

---

### `Win:show()` {#Win-show}

```lua
Win:show()
```

Makes the window visible again after it was hidden with `hide()`.

**Example:**

```lua
win:show()
```

---

### `Win:hide()` {#Win-hide}

```lua
Win:hide()
```

Hides the window without closing it. The window keeps its state
and buffer contents. Call `show()` to bring it back.

A hidden window of any kind takes no space, draws nothing and claims no
keys. It still accepts commands and reports events.

**Example:**

```lua
win:hide()
-- do some work...
win:show()
```

---

### `Win:is_visible()` {#Win-is_visible}

```lua
Win:is_visible()
```

Returns true if the window is both open and visible (not hidden).

**Returns:** (`boolean`) true if visible.


## maki.ui.Buf {#maki-ui-Buf}

A content buffer that holds styled lines of text. Create one with
`maki.ui.buf()` and pass it to `maki.ui.open_win()` to show it in
a floating or split window.

```lua
local buf = maki.ui.buf()
buf:line("hello")
buf:line({ { "world", "bold" } })
```

---

### `Buf:line()` {#Buf-line}

```lua
Buf:line({line})
```

Appends a single line to the end of the buffer. You can pass a
plain string for unstyled text, or a table of `{text, style?}` spans
for rich content. Style can be a named string like "bold" or
"keyword", or an inline table `{fg?, bg?, bold?, italic?, underline?, dim?, strikethrough?, reversed?}`.
Colors accept "#rrggbb", a terminal color name like "blue" or "light-gray",
or a palette index as a string like "4". Names must be spelled exactly,
hyphens included. Named and indexed colors are left for the terminal to
resolve, so they follow the user's palette. To mix a named style with colors
of your own, `maki.ui.theme_style` hands back its parts.

**Parameters:**

- `{line}` (`string|table`) Plain string, or a sequence of spans: `{ {text, style?}, ... }`.

**Example:**

```lua
buf:line("plain text")
buf:line({ { "ERROR", { fg = "#ff0000", bold = true } }, { " something broke" } })
```

---

### `Buf:lines()` {#Buf-lines}

```lua
Buf:lines({lines})
```

Appends several lines at once. Each entry uses the same format as
`buf:line()`, so you can mix plain strings and styled spans.

**Parameters:**

- `{lines}` (`table`) Sequence of line values, each the same format accepted by `buf:line`.

**Example:**

```lua
buf:lines({
  "first line",
  { { "styled ", "bold" }, { "second line" } },
  "third line",
})
```

---

### `Buf:set_lines()` {#Buf-set_lines}

```lua
Buf:set_lines({lines})
```

Replaces every line in the buffer with {lines}. Use this when you
want to redraw the whole buffer, for example after the user toggles
a view.

**Parameters:**

- `{lines}` (`table`) Sequence of line values, each the same format accepted by `buf:line`.

**Example:**

```lua
buf:set_lines({ "new content", "replaces everything" })
```

---

### `Buf:len()` {#Buf-len}

```lua
Buf:len()
```

Returns how many lines the buffer currently holds.

**Returns:** (`integer`) Line count.

**Example:**

```lua
if buf:len() == 0 then
  buf:line("(empty)")
end
```

---

### `Buf:get_lines()` {#Buf-get_lines}

```lua
Buf:get_lines()
```

Returns all lines in the buffer as a Lua table. Each line is a
sequence of `{text, style?}` spans, the same format `buf:line()`
accepts. Useful for reading back content, copying it to another
buffer, or round-tripping through `set_lines()`.

**Returns:** (`table`) Sequence of lines.

**Example:**

```lua
local lines = buf:get_lines()
buf:set_lines(lines) -- round-trip
```

---

### `Buf:on()` {#Buf-on}

```lua
Buf:on({event}, {callback})
```

Registers an event handler on the buffer.

Supported events:
- "click": fires when the user clicks a line. The handler receives
  a click-event table and may yield or mutate the buffer.
- "change": fires synchronously after every mutation (`line`,
  `lines`, `set_lines`). Must not yield.

Calling `on()` again for the same event replaces the previous handler.

**Parameters:**

- `{event}` (`string`) Event name: "click" or "change".
- `{callback}` (`function`) Handler function. For "click", receives a click-event table. For "change", receives no arguments.

**Example:**

```lua
buf:on("click", function(ev)
  maki.ui.flash("Clicked row " .. ev.row)
end)
```

---

### `Buf:click()` {#Buf-click}

```lua
Buf:click({ev})
```

Programmatically fires the buffer's click handler with event {ev}.
Does nothing if no click handler is registered. Useful for testing
or simulating user interaction from code.

**Parameters:**

- `{ev}` (`table`) Click event table passed to the handler.

**Example:**

```lua
buf:click({ row = 1 })
```

---

### `Buf:blit()` {#Buf-blit}

```lua
Buf:blit({fb}, {width}, {height}, {opts?})
```

Replaces the whole buffer with a pixel frame drawn as `"▀"` cells.
Each cell's foreground is the top pixel and its background the
bottom one, so one text line fits two pixel rows. When {height} is
odd the last line leaves its background unset and the terminal
default shows through.

{fb} is a Luau `buffer` of raw pixel bytes in row-major order,
top-left origin. Its size must be exactly
`width * height * bytes_per_pixel` for the chosen format, otherwise
the call throws. A mismatch usually means a wrong width or format,
and an early error beats hunting down a garbled frame.

Formats: "rgb" is the default at 3 bytes per pixel. "rgba" and
"bgra" take 4 bytes per pixel and ignore the 4th byte. "bgra" is
what a little-endian `uint32` holding `0xRRGGBB` looks like in
memory, the layout doomgeneric uses for its framebuffer.

`char` swaps the `"▀"` glyph for another one column wide string,
e.g. `"█"` when only the foreground color should show. The
foreground still comes from the top pixel and the background from
the bottom one, whatever the glyph.

**Parameters:**

- `{fb}` (`buffer`) Raw pixel bytes.
- `{width}` (`integer`) Frame width in pixels, > 0.
- `{height}` (`integer`) Frame height in pixels, > 0.
- `{opts?}` (`table|nil`) Options: `format` = "rgb"|"rgba"|"bgra", `char` = one column wide string.

**Example:**

```lua
local fb = buffer.create(160 * 100 * 3)
buffer.writeu8(fb, (y * 160 + x) * 3, 255) -- red channel
buf:blit(fb, 160, 100)
buf:blit(fb32, 160, 100, { format = "bgra", char = "█" })
```


## maki.uv {#maki-uv}

System and environment utilities, modelled after `vim.uv`.

Provides access to the working directory, home directory, and environment
variables. None of these functions throw.

Filesystem location queries (`cwd`, `os_homedir`) need `fs_read`, while
`os_getenv` reads the process environment, where secrets live, so it needs
`env`.

```lua
local home = maki.uv.os_homedir()
```

---

### `maki.uv.cwd()` {#maki-uv-cwd}

```lua
maki.uv.cwd()
```

Return the current working directory as an absolute path. Like `vim.uv.cwd`.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Returns:** (`string?`) Current working directory, or nil if it cannot be determined.

**Example:**

```lua
local cwd = maki.uv.cwd()
if cwd then print("working in: " .. cwd) end
```

---

### `maki.uv.os_homedir()` {#maki-uv-os_homedir}

```lua
maki.uv.os_homedir()
```

Return the current user's home directory. Like `vim.uv.os_homedir`.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Returns:** (`string?`) Home directory path, or nil if it cannot be determined.

**Example:**

```lua
local home = maki.uv.os_homedir() -- e.g. "/home/user"
```

---

### `maki.uv.os_getenv()` {#maki-uv-os_getenv}

```lua
maki.uv.os_getenv({name})
```

Look up the environment variable {name}. Like `vim.uv.os_getenv`.
Returns nil when the variable is not set.

Requires the `env` [plugin permission](#plugin-permissions).

**Parameters:**

- `{name}` (`string`) Name of the environment variable.

**Returns:** (`string?`) Variable value, or nil if not set.

**Example:**

```lua
local editor = maki.uv.os_getenv("EDITOR") or "vi"
```


## maki.yaml {#maki-yaml}

YAML encoding and decoding. Works the same way as `maki.json`,
but for YAML formatted strings.

```lua
local t = maki.yaml.decode("greeting: hello")
print(t.greeting)
```

---

### `maki.yaml.encode()` {#maki-yaml-encode}

```lua
maki.yaml.encode({value})
```

Turn a Lua value into a YAML string. Most Lua types work, but
circular references will return an error.

**Parameters:**

- `{value}` (`any`) Lua value to encode.

**Returns:** (`string?`, `string?`) YAML string, or nil plus an error.

**Example:**

```lua
local s, err = maki.yaml.encode({ name = "maki", tags = { "ai", "agent" } })
print(s)
```

---

### `maki.yaml.decode()` {#maki-yaml-decode}

```lua
maki.yaml.decode({str})
```

Parse a YAML string into a Lua value. Mappings become tables and
sequences become 1-indexed arrays.

**Parameters:**

- `{str}` (`string`) YAML string to decode.

**Returns:** (`any?`, `string?`) Decoded value, or nil plus an error.

**Example:**

```lua
local t, err = maki.yaml.decode("name: maki\nversion: 1")
print(t.name) -- maki
```


## Shared helper modules

These ship inside maki; `require` them from any plugin. Small modules are
shown as full source, larger ones as their public interface.

### `require("maki.color")`

```lua
local M = {}

function M.lerp(from, to, t)
  local fr, fg, fb = from:match("^#(%x%x)(%x%x)(%x%x)$")
  local tr, tg, tb = to:match("^#(%x%x)(%x%x)(%x%x)$")
  if not fr or not tr then
    return nil
  end
  fr, fg, fb = tonumber(fr, 16), tonumber(fg, 16), tonumber(fb, 16)
  tr, tg, tb = tonumber(tr, 16), tonumber(tg, 16), tonumber(tb, 16)
  local r = math.floor(fr + (tr - fr) * t + 0.5)
  local g = math.floor(fg + (tg - fg) * t + 0.5)
  local b = math.floor(fb + (tb - fb) * t + 0.5)
  return string.format("#%02x%02x%02x", r, g, b)
end

function M.dim(color, factor)
  local background = maki.ui.theme_style("background")
  local bg = background and background.bg
  return bg and M.lerp(color, bg, factor)
end

return M
```

### `require("maki.dir_listing")`

```lua
-- Shared directory listing for index and list plugins, so every caller
-- shows a directory the same way. Listing also loads the directory's
-- instruction files onto the call.
function M.list(path, ctx)
function M.view(text, ctx)
```

### `require("maki.fuzzy_replace")`

```lua
M.NO_MATCH = "old_string not found in file"
M.MULTIPLE_MATCHES = "old_string matches multiple locations; add surrounding context to make it unique"
M.EMPTY_OLD_STRING = "old_string must not be empty"

-- Replace {old_string} with {new_string} in {content}, tolerating small
-- whitespace and indentation drift. Returns the new content, or nil plus
-- one of the error constants above.
function M.replace(content, old_string, new_string, replace_all)
```

### `require("maki.list_picker")`

```lua
-- Draws the filter query and its blank spacer into {lines}, pins that height on
-- {win} and returns it, which is also the first scrollable line. Drawing and
-- pinning belong together: a query that wraps, or one pasted with a newline,
-- makes the header taller than a picker would guess, and a reserved_top guessed
-- elsewhere then mis-scrolls the list.
function ListPicker.render_header(win, lines, input, prefix, inner)

-- Open a fuzzy-filter picker in a floating window and block until the user
-- decides.
--
-- {items} is a list of strings or of
-- { label, detail?, section?, section_detail? } tables. A detail is a string, or
-- a list of { text, style } parts when the right of a row needs more than one
-- color. Mark one part `elastic = true` and that is the part a narrow row cuts,
-- leaving the parts after it whole.
--
-- {opts}:
--   title, footer, cursor (initial index)
--   submit_keys: extra submit keys besides <CR>
--   action_keys: keys that close the picker and report themselves, like { "R" }
--     for a refresh binding. Use uppercase keys, lowercase ones keep feeding
--     the filter
--   live_keys: { [key] = function(item|nil) -> items|nil }, keys that swap the
--     list in place, keeping the typed query. Called with the selected item, or
--     nil when nothing matches, and returning nil leaves the list alone.
--     Handlers run inside the render loop, so keep them cheap and hand anything
--     slow to an action_key
--   key: function(item) -> string|nil, a row's identity. Rows sharing the
--     selected row's key are tinted, and the cursor follows its key across a
--     live swap
--
-- Keys you pass go through `maki.keymap.normalize`, so `"<Enter>"` and
-- `"<CR>"` are the same binding. An invalid key is dropped with a warning.
--
-- Returns { type = "choice"|"delete", index, item },
-- { type = "key", key, index?, item? } or { type = "close" }. Prefer {item},
-- since {index} points into an {items} a live swap may have replaced.
function ListPicker.open(items, opts)
ListPicker.split_words = split_words
ListPicker.matches = matches
ListPicker.highlight_spans = highlight_spans
ListPicker.range_spans = range_spans
```

### `require("maki.output_limits")`

```lua
-- Shared per-tool output limit options, so the tools that support them
-- cannot drift apart.
M.DEFAULT_MAX_LINE_BYTES = DEFAULT_MAX_LINE_BYTES
function M.extend(spec)

--- Returns max_lines, max_bytes: tool override when set, agent-wide otherwise.
--- Both nil in full mode.
function M.resolve(opts, ctx)

--- opts.max_line_bytes, or 0 (no limit) in full mode.
function M.line_bytes(opts, ctx)

--- Cuts {text} to the limits keeping its head and its tail, since on a long
--- build or test run the errors and the summary sit at the end. Each end gets
--- half of every limit. A nil limit is unlimited. The marker between the two
--- notes how many lines and bytes it dropped.
function M.cut_middle(text, max_lines, max_bytes)
```

### `require("maki.partial")`

```lua
-- When a tool is cut short, it still hands back what it printed. The marker
-- tells the model that output is real but unfinished. One home for the
-- wording and the painting, so every tool says it the same way.

--- Close {view} on the marker and build the tool reply. {out} is everything
--- the tool streamed, already truncated; empty means the view still shows a
--- placeholder to drop. {reason} is a cancel-hook reason ("cancelled" |
--- "timeout"). {started_at} is the `os.time()` the tool began at.
function M.cut(view, out, reason, timeout_secs, started_at)
```

### `require("maki.provider_parse")`

```lua
-- Typed readers for provider model-list and usage JSON, for use in
-- `list_models` and `fetch_usage` hooks.
--
-- `8192` and `8192.0` both count as whole numbers. A JSON null decodes to nil,
-- and in an array it stops `ipairs`. Numbers above 2^53 come back rounded.

--- A whole, non-negative number up to 2^64, or nil.
function M.as_u64(value)

--- A whole, non-negative number that fits a u32, or nil.
function M.as_u32(value)

--- A number, or nil.
function M.as_f64(value)

--- A boolean, or nil.
function M.as_bool(value)

--- A model row's `pricing`, converted from per-token to per-million-token
--- dollars. Nil unless both `input` and `output` are given, so a partial price
--- never reads as free. A missing cache price is 0.
function M.pricing(input, output, cache_write, cache_read)

--- Maps each `body.data` element through `parse_row`, drops nils, keeps the
--- first row per id, and sorts by id. Returns an empty list when `data` is
--- not an array.
function M.models(body, parse_row)
```

### `require("maki.scroll")`

```lua
-- Relative scrolling on top of maki.fn.winsaveview / winrestview.
-- Positive {delta} scrolls down, negative up. Returns (true, nil) or (nil, err).
local function scroll(delta)
  local view, err = maki.fn.winsaveview()
  if not view then
    return nil, err
  end
  return maki.fn.winrestview({ topline = view.topline + delta })
end

return scroll
```

### `require("maki.shorten_path")`

```lua
local function normalize_sep(s)
  return s:gsub("\\", "/")
end

local function shorten_path(path)
  local p = normalize_sep(path)
  local cwd = maki.uv.cwd()
  if cwd then
    cwd = normalize_sep(cwd)
    if p:sub(1, #cwd + 1) == cwd .. "/" then
      local rel = p:sub(#cwd + 2)
      return rel == "" and "." or rel
    end
  end
  local home = maki.uv.os_homedir()
  if home then
    home = normalize_sep(home)
    if p:sub(1, #home + 1) == home .. "/" then
      local rel = p:sub(#home + 2)
      return rel == "" and "~" or "~/" .. rel
    end
  end
  return path
end

return shorten_path
```

### `require("maki.test_helpers")`

```lua
-- Shared test helpers for Lua plugin specs.
--
-- Provides a lightweight test harness: `case` wraps each block in pcall so a
-- single failure does not abort the rest of the suite. Failures are collected
-- and surfaced by `report()` at the end.
function M.case(name, fn)
function M.eq(actual, expected, msg)
function M.has(s, substr, msg)
function M.mktmpdir(prefix)
function M.rmtree(dir)
function M.report()
```

### `require("maki.text_input")`

```lua
-- TextInput: multi-line editable buffer with a byte-offset cursor.
--
-- Invariants enforced everywhere:
--   * `line` is 1-based and indexes a line that always exists.
--   * `col` is a byte offset inside `lines[line]`, always on a UTF-8 codepoint
--     boundary, so `lines[line]:sub(1, col)` is a complete UTF-8 prefix.
--   * No line ever contains a literal newline; newlines split into rows.
--
-- Parents OWN their keys. `handle_key` returns one of R.IGNORED / R.MOVED /
-- R.CHANGED. Parent dispatchers must filter their own keys (`<Esc>`, `<C-c>`,
-- submit keys, etc.) BEFORE forwarding, because `handle_key` claims any key
-- it can interpret. `<C-a>` is bound to move-home, so a parent that wants it
-- for "select all" must intercept first.
--
-- Keys are in canonical notation, as `win:recv` delivers them. Splitting a
-- line is the `input:split_line()` method rather than a pseudo-key, so every
-- key in KEYMAP is one a terminal can send.
--
-- IGNORED is returned when the buffer literally cannot act (backspace at
-- (1, 0), right at end of buffer, etc.). Parents can use that signal to fall
-- through to their own logic.
--
-- Parity cases live in plugins/lib/tests/spec.lua (TRACE_CASES). Add one
-- whenever you change handle_key semantics.
TextInput.Result = R
function TextInput.new()
function TextInput:value()
function TextInput:is_empty()
function TextInput:line_count()
function TextInput:clear()

-- Returns the codepoint right before the cursor as a string, or nil at the
-- start of a line. Lets callers peek backwards (e.g. "is the previous char
-- a backslash?") without touching internal indices.
function TextInput:char_before_cursor()
function TextInput:insert_text(text)
function TextInput:insert_char(c)
function TextInput:insert_space()
function TextInput:split_line()
function TextInput:remove_char()
function TextInput:delete_char()
function TextInput:remove_word_before()
function TextInput:delete_word_after()
function TextInput:kill_to_end_of_line()
function TextInput:move_left()
function TextInput:move_right()
function TextInput:move_up()
function TextInput:move_down()
function TextInput:move_home()
function TextInput:move_end()
function TextInput:move_word_left()
function TextInput:move_word_right()
function TextInput:handle_key(key)

-- Wrap lines to {width} with {prefix} before the first row. Returns
-- { lines = styled lines, cursor_row = 1-based row holding the cursor }.
function TextInput:render(prefix, prefix_width, width)
```

### `require("maki.toast")`

```lua
-- Corner toast notifications built on floating windows. `maki.ui.flash` gives
-- you one line in the status area. A toast stays up long enough to read, can
-- carry a title, and stacks under the toasts already on screen.

-- Show {text} as a toast, up to 5 lines of it. {opts}: title (string),
-- timeout_secs (integer, default 4). Returns right away and the toast
-- dismisses itself when the time is up.
function Toast.show(text, opts)
```

### `require("maki.tool_view")`

```lua
-- The shared truncate/expand body that tool plugins render through.
--
-- Click handlers get `ev.row`, a 1-based line in this buf; 0 means the
-- click landed outside it (the header). The handler lives on the buf
-- itself, so any wrapper of the same buf (a batch child's foreign handle)
-- reaches the same toggle. Expansion is never stored: the UI records
-- clicked rows and replays them through `restore` in order, so `toggle`
-- stays a pure flag flip + re-render, deterministic across replays.
-- Async highlighting goes through `maki.async.run`; during restore the
-- runtime runs those tasks inline before snapshotting.

-- Right aligned, so the content column stays put when a number gains a digit.
-- Formats the number alone, callers add their own separator.
function ToolView.line_nr_fmt(max_line_nr)

-- opts: max_lines (default 80) shown while collapsed, keep "head"|"tail"
-- (default "tail"), max_expand_lines (default 2000) kept for expansion,
-- max_line_bytes (optional) per-line byte cap applied at render time.
function ToolView.new(buf, opts)
function ToolView:set_header(lines)
function ToolView:clear()
function ToolView:append(line)
function ToolView:append_text(text)

-- Append {content} with line numbers, then syntax-highlight it for {ext}
-- asynchronously. Returns false when {content} is empty.
function ToolView:set_highlight(content: string, ext)

-- Content rows on screen, for callers with their own per-row click targets. A
-- single hidden line is drawn as itself instead of a notice, so it counts as
-- content too. Rows line up with `all_lines` under keep = "head"; keep = "tail"
-- prints its notice first and shifts them.
function ToolView:visible_count()
function ToolView:toggle()
function ToolView:flush()
function ToolView:update_line(all_idx, line)

-- Call once after the last append so the collapsed notice renders.
function ToolView:finish()
function ToolView.restore_lines(lines, opts)

-- Rebuild a collapsed view from a tool's saved llm_output, click-to-toggle
-- wired. For `restore` hooks.
function ToolView.restore(output, opts)

-- Same, for tools whose live output goes through markdown (`format =
-- "markdown"`). {opts.width} is the wrap width and defaults to the body
-- column of the current terminal. Errors stay plain, as they do live.
function ToolView.restore_markdown(output, is_error, opts)
```

### `require("maki.truncate")`

```lua
--- require("maki.truncate")(text, max_lines, max_bytes)
--- Cuts {text} to the limits and notes how many bytes it dropped. A nil limit
--- is unlimited.
```

