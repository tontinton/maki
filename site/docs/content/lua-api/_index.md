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

- `fs_read`: reading files, and locating the directories maki keeps them in
- `fs_write`: creating, changing, and removing files
- `net`: outbound network requests
- `run`: starting processes
- `env`: reading the process environment, where secrets live

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
| [`maki.agent`](#maki-agent) | Subagent primitives for plugins that need to talk to an LLM. |
| [`maki.agent.Session`](#maki-agent-Session) | A subagent session with its own conversation history. |
| [`maki.async`](#maki-async) | Tools for running things concurrently in Lua plugins. |
| [`maki.async.Semaphore`](#maki-async-Semaphore) | A counting semaphore for limiting how many tasks run at once. |
| [`maki.async.Permit`](#maki-async-Permit) | One slot in a semaphore, obtained from `Semaphore:acquire()`. |
| [`maki.base64`](#maki-base64) | Base64 encoding and decoding, modelled after `vim.base64`. |
| [`maki.env`](#maki-env) | Paths to maki's own directories (config, state, logs, legacy). |
| [`maki.fn`](#maki-fn) | Process and environment helpers, modeled after Neovim's `vim.fn` job |
| [`maki.fs`](#maki-fs) | File-system utilities, modelled after `vim.fs` and `vim.uv`. |
| [`maki.image`](#maki-image) | Small building blocks for working with images: probe metadata, decode |
| [`maki.image.Image`](#maki-image-Image) | A decoded image you can inspect, resize, and re-encode. |
| [`maki.interpreter`](#maki-interpreter) | Run Python code in a memory-safe, time-limited sandbox. |
| [`maki.json`](#maki-json) | JSON encoding, decoding, and schema validation. |
| [`maki.json.SchemaValidator`](#maki-json-SchemaValidator) | A compiled JSON Schema validator. |
| [`maki.keymap`](#maki-keymap) | Key mappings, modeled after `vim.keymap`. |
| [`maki.log`](#maki-log) | Structured logging for plugins. |
| [`maki.model`](#maki-model) | The model behind the focused session. |
| [`maki.net`](#maki-net) | HTTP client for fetching web content. |
| [`maki.session`](#maki-session) | Host session primitives. |
| [`maki.Timer`](#maki-Timer) | Handle returned by `maki.defer_fn`. |
| [`maki.task`](#maki-task) | The subagents of the focused session and their transcripts. |
| [`maki.text`](#maki-text) | Text transformation utilities. |
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

Run {callback} after {ms} milliseconds, on the Lua thread and outside
any task scope. The timer does not hang off the caller's cancel token
or the 60 second `async.run` deadline, so the callback still fires
once the tool call that scheduled it is over. That is what a toast
needs to dismiss itself, and the difference from `maki.async.sleep`.

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
`{opts.title}` in front of the message when you pass one. A run with
no UI, such as `maki -p` or the sdk, logs the notice instead of
dropping it.

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

**Returns:** (`table`) Package records with `spec`, `path`, `rev`, and `active`.


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

**Parameters:**

- `{spec}` (`table`) Tool specification:
  - `name` (`string`) Required. ASCII identifier, up to 64 chars ([a-zA-Z_][a-zA-Z0-9_]*).
  - `description` (`string`) Required. Non-empty description shown to the model.
  - `schema` (`table`) Required. JSON Schema object describing the tool's input parameters.
  - `handler` (`function`) Required. Called with `(input, ctx)` when the tool is invoked.
    Must return a string or a table with any of these fields:
    - `llm_output` (`string`) Text sent to the model.
    - `is_error` (`boolean`) When true, the result is treated as an error.
    - `content` (`string`) Alias for llm_output (legacy).
    - `body` (`BufHandle`) Rich rendered body shown in the UI.
    - `header` (`BufHandle`) One-line header shown before the body.
    - `format` (`string`) "plain" (default) or "markdown".
    - `annotation` (`string`) Short label shown next to the tool call.
    - `written_path` (`string`) Path of a file written by the tool.
    - `diff_path` (`string`) Path for a diff output block.
    - `diff_before` (`string`) Before text of the diff.
    - `diff_after` (`string`) After text of the diff.
    - `image` (`table`) { media_type: string, data: string } base64 image.
    - `instructions` (`table`) Array of { path, content } blocks injected as context.
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
  - `permission` (`string`) Required with `permission_scopes`. The capability the tool exposes to the model: "fs_read", "fs_write", "net", "run", or "env". Your plugin must hold it, and so must any plugin that pre-approves this tool.
  - `mutable_path` (`string`) Schema field name (type: string) for the primary path the tool writes. Required with `permission = "fs_write"`. Declaring it is what gets the tool, from the dispatcher and never from the handler: serialization of concurrent calls on that file, the stale-read rejection, the plan-mode block, and the permission boundary check.
  - `start_annotation` (`string|table`) Schema field used to annotate the start header with a count (string) or timeout (`{ field, kind="timeout" }`).

**Example:**

```lua
maki.api.register_tool({
  name = "word_count",
  description = "Count words in a file.",
  kind = "read",
  schema = {
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
already do itself. A deny needs no permission.

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

**Parameters:**

- `{spec}` (`table`) Hint specification:
  - `slot` (`string`) Required. Aggregate slot name (e.g. "tool_usage", "general").
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

Set a singleton prompt slot. Only one plugin owns each singleton slot at a
time, so calling this replaces any previous value from your plugin.

Use this for slots like "identity" or "tone" where a single coherent value
makes more sense than combining fragments. For aggregate slots like
"tool_usage", use `register_prompt_hint` instead.

Throws if you pass an aggregate slot name.

**Parameters:**

- `{spec}` (`table`) Spec fields mirror `register_prompt_hint`:
  - `slot` (`string`) Required. Singleton slot name (e.g. "identity", "tone").
  - `content` (`string|function`) Required. Static text or a `function()` returning a string. Max 1 MiB.
  - `prompt` (`string|string[]`) Optional. Restrict to specific prompt ids.

**Example:**

```lua
maki.api.set_prompt({
  slot = "tone",
  content = "Be concise. No filler words.",
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

**Parameters:**

- `{name}` (`string`) Exact tool name.

**Returns:** (`table|nil`) Tool entry with fields { name, schema, audiences, kind?, header?, restore? }, or nil if not found.

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

**Parameters:**

- `{cmdline}` (`string`) Command line, e.g. `"/new"` or `"/cd ~/src"`.

**Returns:** (`boolean|nil`, `string|nil`) `true` once dispatched, or nil and an error message for an unknown command.

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
`"SessionFocusChanged"`, `"SessionStatusChanged"`, `"TaskStatusChanged"`,
and `"ModelChanged"`. Plugins can also fire their own events with
`exec_autocmds`.

Every host event carries `data.session_id`. For `"SessionReset"` and
`"SessionEnd"` that is the session being left behind, the other events
name the session now running or focused. What each event adds:

- `"ToolStart"`, `"ToolDone"`: `data.tool_id` and `data.tool`.
- `"TurnEnd"`: `data.reason` (`"finished"`, `"max_tokens"`,
  `"max_turns"`, or `"cancelled"`), `data.usage` (four token fields,
  cache included), `data.cost`, `data.list_cost`, `data.context_size`,
  `data.context_window`, and `data.num_turns` (model round-trips the
  turn took). `list_cost` is the un-subsidised list price and `cost` is
  the real bill, so a budget plugin charges against whichever one it
  wants.
- `"AutoCompacting"`: `data.context_size` and `data.context_window` at
  trigger time.
- `"CompactionDone"`: `data.context_size_before`,
  `data.context_size_after`, and `data.context_window`.
- `"PlanReady"`: `data.path`, the absolute path of the plan file the
  agent just wrote. Fires once per draft.
- `"SessionFocusChanged"`: `data.previous_session_id`, absent on the
  first focus at startup.
- `"SessionStatusChanged"`: `data.status` (`"working"`, `"needs_input"`,
  or `"idle"`), `data.title`, and `data.focused` (boolean).
- `"TaskStatusChanged"`: `data.id`, `data.name`, and `data.status`
  (`"working"`, `"done"`, or `"error"`), when a subagent starts or
  changes status. A task that comes back from disk already finished
  stays quiet, so reloading a session does not replay old tasks.
- `"ModelChanged"`: `data.model` in the shape `maki.model.get` returns,
  plus `data.previous_spec`. Picking the model already in use stays
  quiet, and so does startup.

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

### `maki.api.declare_slot()` {#maki-api-declare_slot}

```lua
maki.api.declare_slot({name}, {default})
```

Create a named extension point owned by your plugin. You provide a
{default} function, and other plugins can wrap it with layers using
`set_slot`. The returned callable runs the full chain: outermost
layer first, then inward, ending at {default}.

Throws if another plugin already owns a slot with the same {name}, or
if {name} starts with `"tool."`, which the host fires itself.

The chain is async: the default and every layer may park (`maki.fs.*`,
`maki.fn.jobwait`, `maki.agent.call_tool`, ...), and so does the
returned callable. Call it from a tool handler, a command, or an
autocmd, rather than from a `header` or `restore` function, which
cannot wait. The chain runs in your task, so cancelling the caller
cancels the layers it is waiting on.

**Parameters:**

- `{name}` (`string`) Unique slot name, e.g. `"myplugin.render"`.
- `{default}` (`function`) Default implementation, called when no layers wrap it.

**Returns:** (`function`) Callable that dispatches through all layers.

**Example:**

```lua
local render = maki.api.declare_slot("myplugin.render", function(text)
  return text:upper()
end)
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

Maki fires two slots per tool itself: `tool.<name>.input` before
permissions look at the call, and `tool.<name>.output` on the text it
produced. Both take `function(prev, value, ctx)` and answer with a
table to replace the value, nothing to leave it alone, or
`nil, reason` to stop the call. Wrapping one costs the capability the
tool declares, and a tool declaring none costs every permission. See
[Hooks](/docs/hooks/).

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

**Returns:** (`table`) Map of slot name to `{ owner, declared, fillers }`.

**Example:**

```lua
for name, info in pairs(maki.api.get_slots()) do
  print(name, info.owner, info.declared)
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
    the parent tier so you cannot escalate.
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

Build a system prompt from a built-in template. Environment variables like
`{cwd}` are substituted automatically. Use this when you need a ready-made
prompt for a subagent session.

**Parameters:**

- `{ctx}` (`LuaCtx`) Agent context.
- `{opts}` (`table`) Required fields:
  - `prompt_id` (`string`) one of `"research"`, `"general"`, `"system"`.

  Optional fields:

  - `instructions` (`string|boolean?`) extra text appended to the prompt.
    `true` loads instructions from the project `.maki/instructions` file.
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
maki.agent.tools({ctx}, {opts})
```

Get the list of tool definitions for a given audience. Pass the result
straight into `maki.agent.session()` or use it to inspect what tools are
available.

**Parameters:**

- `{ctx}` (`LuaCtx`) Agent context.
- `{opts}` (`table`) Required fields:
  - `audience` (`string`) tool audience filter, e.g. `"general"`, `"subagent"`,
    `"general_sub"`.

  Optional fields:

  - `only` (`string[]?`) include only these tool names.
  - `except` (`string[]?`) exclude these tool names.
  - `workflow` (`boolean?`) use workflow-mode descriptions. Default: `false`.
  - `spec` (`string?`) evaluate capability exclusions against this model spec.
  - `mcp` (`boolean?`) describe tools as if MCP is reachable. Default: `true`.
    Pass what you pass to `maki.agent.session()`. Otherwise the descriptions
    advertise MCP tools that the session has no way to call.

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
  - `on_live_buf` (`function?`) called with a `BufHandle` for each live buffer
    the tool publishes. Must not yield.
  - `on_annotation` (`function?`) called with an annotation string for each
    annotation event. Must not yield.
  - `on_usage` (`function?`) called with a formatted cumulative token usage
    string. Must not yield.

**Returns:** (`string?`, `string?`) Tool output text, or `(nil, err)` on failure.

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
maki.agent.session({ctx}, {opts})
```

Create a new subagent session. The session inherits the parent model and
MCP handle unless you override them. You get back a `Session` object that
you can send messages to with `:prompt()`.

This is the main way to spin up a sub-conversation with its own history
and tool set.

**Parameters:**

- `{ctx}` (`LuaCtx`) Agent context.
- `{opts}` (`table`) Optional fields:
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
  - `mcp` (`boolean?`) give the session access to MCP tools. Their
    definitions are injected automatically each turn (deferred behind
    `tool_search`), so don't put MCP definitions in `tools`. The session
    starts with no loaded tools of its own. Default: `true`.
  - `thinking` (`string|integer?`) thinking mode: `"off"`, `"adaptive"`, an
    effort level (`"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`,
    `"max"`), or a budget integer (token count). Inherits parent setting
    if omitted.
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


## maki.agent.Session {#maki-agent-Session}

A subagent session with its own conversation history.

Create one with `maki.agent.session()`, then send messages with
`:prompt()`. The session remembers previous turns, so you can have
a multi-step conversation.

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
error and a `{ text = <what it streamed> }` table.

**Example:**

```lua
local r, err = sess:prompt("What files are in this project?")
if err then error(err) end
print(r.text)
print(r.input_tokens .. " input, " .. r.output_tokens .. " output tokens")
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


## maki.async {#maki-async}

Tools for running things concurrently in Lua plugins.

Use `run` to fire off background tasks, `gather` or `join` to run
several functions at once, and `semaphore` to limit concurrency.
The `await` and `wrap` helpers bridge callback-based APIs into
coroutine-friendly calls.

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

Fire off a function as a new async task. It runs in the background and
you do not wait for it. If you need the result, pass an {on_finish}
callback.

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

### `maki.async.sleep()` {#maki-async-sleep}

```lua
maki.async.sleep({ms})
```

Suspend the calling task for {ms} milliseconds. The plugin thread is
never blocked, so other tasks and the UI keep running, and a cancel
still lands while you sleep.

For a timer that has to outlive the tool call that started it, such
as a toast dismissing itself, use `maki.defer_fn`.

**Parameters:**

- `{ms}` (`integer`) Milliseconds to sleep.

**Example:**

```lua
maki.async.run(function()
  maki.async.sleep(4000)
  win:close()
end)
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
Throws if {str} is not valid Base64.

**Parameters:**

- `{str}` (`string|buffer`) Base64-encoded text.

**Returns:** (`string`) Decoded bytes as a string.

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
or `cmd /C` on Windows; a table is spawned as argv, with no shell in
between (nothing in it can be read as a redirect, a pipe, or `$(...)`).
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

**Returns:** (`integer`) Job id.

**Example:**

```lua
local id = maki.fn.jobstart({ "rg", "--json", pattern, dir }, {
  on_stdout = function(_, line) print(line) end,
  on_exit = function(_, code) print("exit: " .. code) end,
})
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
truncated. Returns `nil` if the job does not finish before the timeout.

While waiting, the job's `on_stdout`, `on_stderr`, and `on_exit`
callbacks fire as events arrive (like Neovim), so you can stream
output into a buffer while parked here. An already-exited
session-owned job answers from its snapshot and fires no callbacks.
Task and plugin jobs leave the store on exit, so waiting after that
is an error.

Requires the `run` [plugin permission](#plugin-permissions).

**Parameters:**

- `{job_id}` (`integer`) Job id returned by `jobstart`.
- `{timeout_ms?}` (`integer?`) Maximum wait in milliseconds (default 30000).

**Returns:** (`table?`) `{ stdout, stderr, exit_code, truncated }`, or nil on timeout.

**Example:**

```lua
local id = maki.fn.jobstart("echo hello")
local result = maki.fn.jobwait(id, 5000)
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
maki.fs.read({path})
```

Read the entire file at {path} as a UTF-8 string.
If the file contains bytes that are not valid UTF-8, this function throws.
Use `read_bytes` for binary files.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{path}` (`string`) Absolute or relative file path. `~/` is expanded to the home directory.

**Returns:** (`string?`, `string?`) File contents, or nil plus an error message.

**Example:**

```lua
local text, err = maki.fs.read("config.toml")
if err then
  maki.log.warn("could not read config: " .. err)
  return
end
```

---

### `maki.fs.read_bytes()` {#maki-fs-read_bytes}

```lua
maki.fs.read_bytes({path})
```

Read the entire file at {path} as raw bytes, returned as a Luau buffer.
Useful for binary files or when you need to pass the data to `maki.base64.encode`.

Requires the `fs_read` [plugin permission](#plugin-permissions).

**Parameters:**

- `{path}` (`string`) Absolute or relative file path. `~/` is expanded to the home directory.

**Returns:** (`buffer?`, `string?`) File bytes as a Luau buffer, or nil plus an error message.

**Example:**

```lua
local buf, err = maki.fs.read_bytes("image.png")
if err then return end
local encoded = maki.base64.encode(buf)
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

Find files matching one or more glob patterns.
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
- `{opts?}` (`table?`) `path` (string): search root. `include` (string): file glob filter (e.g. `"*.rs"`). `context_before` / `context_after` (integer): context lines around matches. `limit` (integer): max match groups. `max_line_bytes` (integer): skip lines longer than this.

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
Images larger than 50 megapixels are rejected to prevent memory bombs.

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

A decoded image you can inspect, resize, and re-encode.

Get one from `maki.image.decode()`. The image data lives in memory
until the handle is garbage collected.

---

### `Image:width()` {#Image-width}

```lua
Image:width()
```

Get the width of the image in pixels.

**Returns:** (`integer`) Width in pixels.

---

### `Image:height()` {#Image-height}

```lua
Image:height()
```

Get the height of the image in pixels.

**Returns:** (`integer`) Height in pixels.

---

### `Image:resize()` {#Image-resize}

```lua
Image:resize({max_w}, {max_h})
```

Shrink the image to fit inside {max_w} x {max_h}, keeping the aspect
ratio. If the image already fits, it is returned as-is. Never upscales.

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
images for sending over the network or writing to disk.

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
booleans, and nil all work. Functions and userdata cannot be
serialized.

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
maki.json.decode({str})
```

Parse a JSON string into a Lua value. Objects become tables and
arrays become 1-indexed sequences.

**Parameters:**

- `{str}` (`string`) JSON string to decode.

**Returns:** (`any?`, `string?`) Decoded value, or nil plus an error.

**Example:**

```lua
local t, err = maki.json.decode('{"x": 42}')
print(t.x) -- 42
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

Key mappings, modeled after `vim.keymap`. If you have written a
Neovim keymap plugin before, this will feel familiar.

```lua
maki.keymap.set("n", "<C-t>", function()
  print("hello")
end, { desc = "Say hello" })
```

---

### `maki.keymap.set()` {#maki-keymap-set}

```lua
maki.keymap.set({mode}, {lhs}, {rhs}, {opts?})
```

Bind a key to a Lua function, just like `vim.keymap.set`. Only
normal mode (`"n"`) is supported right now. If {lhs} is already
mapped, the old binding is replaced and a warning is logged.

**Parameters:**

- `{mode}` (`string`) Mode letter. Currently only `"n"` is accepted.
- `{lhs}` (`string`) Key in Vim notation, e.g. `"<C-t>"`, `"<Space>"`, `"a"`.
- `{rhs}` (`function`) Called when the key is pressed.
- `{opts?}` (`table?`) Options:
  - `desc` (`string`) short description shown in the keymap list.

**Example:**

```lua
maki.keymap.set("n", "<C-t>", function()
  print("toggle!")
end, { desc = "Toggle panel" })
```

---

### `maki.keymap.del()` {#maki-keymap-del}

```lua
maki.keymap.del({mode}, {lhs})
```

Remove the mapping for {lhs} in {mode}. Does nothing if no mapping
exists for that key.

**Parameters:**

- `{mode}` (`string`) Mode letter (reserved for future modes).
- `{lhs}` (`string`) Key to unmap, in Vim notation.

**Example:**

```lua
maki.keymap.del("n", "<C-t>")
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

**Returns:** (`table|nil`, `string|nil`) `{spec, id, provider, thinking, fast,
  supports_thinking, supports_fast}`, or nil and an error.

**Example:**

```lua
local m = maki.model.get()
if m.spec ~= "anthropic/claude-opus-4-6" then ... end
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
maki.keymap.set("n", "<M-t>", function() maki.model.set({ thinking = "" }) end)
```


## maki.net {#maki-net}

HTTP client for fetching web content. All traffic goes over HTTPS
(plain HTTP is upgraded). Private and metadata IP addresses are
blocked to prevent SSRF, including after a redirect. Hosts listed in
the `net.allowed_private_hosts` config option are exempt.
Failed requests (5xx) are retried automatically.

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

{opts} fields:
  `method` (string) HTTP verb (default `"GET"`).
  `headers` (table) Header name/value pairs.
  `body` (string) Request body.
  `timeout` (integer) Timeout in seconds, max 120 (default 30).
  `max_bytes` (integer) Max response size in bytes (default 5 MB).
  `retry` (integer) Retries on 5xx errors (default 3).

The response table has three fields: `body` (string), `status`
(integer), and `content_type` (string).

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


## maki.session {#maki-session}

Host session primitives. The interactive UI can run several sessions
at once; these functions let plugins list, create, focus, rename, and
delete them. Session management returns `nil, "no interactive UI
attached"` without a UI. `notify` instead targets a live agent mailbox
directly, so it also works under ACP and SDK frontends.

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

Returns the id of the currently focused session.

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
status. Reads the focused session, or the one you name in `session` when
you act on a background tab.

The returned table:
```
{
  id, cwd, model, mode = "build" | "plan",
  status = "idle" | "working" | "needs_input",
  focused, updated_at,
  usage = { input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens },
  context_size, context_window,
  cost,
  queue = { count }, -- nil under headless drivers
  title,             -- nil under headless drivers
}
```

`usage` and `cost` include subagent spend. `context_size` is the main
session's own, since a subagent runs its own window. There is no
`list_cost` here because `cost` is re-settled from stored usage when a
session resumes and list price is not stored, so per-turn list price
lives on the `TurnEnd` autocmd instead.

**Parameters:**

- `{opts?}` (`table?`) `session` (string?) Session id; defaults to focused.

**Returns:** (`table|nil`, `string|nil`) Snapshot table, or nil and an error.

**Example:**

```lua
local s = maki.session.read()
if s.context_size > s.context_window * 0.8 then
  maki.ui.notify("context is nearly full")
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

Starts a new session in the current project.

**Parameters:**

- `{opts?}` (`table?`) Optional fields: prompt (string) first user message

  to submit right away; focus (boolean) switch the UI to the new session.


**Returns:** (`string|nil`, `string|nil`) New session id, or nil and an error.

**Example:**

```lua
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

**Parameters:**

- `{text}` (`string`) What to report. Must not be blank.
- `{opts?}` (`table`) Options:
  - `session` (`string`) id of a live session.
  - `wake` (`boolean`) start a TUI turn when it next becomes idle (default false).

**Returns:** (`boolean|nil`, `string|nil`) true, or nil and an error.

**Example:**

```lua
maki.session.notify("[monitor] deploy failed", { session = id, wake = true })
```

---

### `maki.session.set_title()` {#maki-session-set_title}

```lua
maki.session.set_title({opts})
```

Renames a session, live or stored.

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
`maki.session.live()` already reports that. The rest are subagents, keyed by
the tool call that spawned them.

**Returns:** (`table|nil`, `string|nil`) Array of `{id, name, focused, status?}` where
  `status` is `"working"`, `"done"`, or `"error"`, or nil and an error.

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

Deletes a finished task and its transcript, so a reload cannot bring it
back. The main chat and still-running tasks are refused.

If the deleted task was the focused one, focus falls back to the previous
chat (or the main chat if the deleted task was first). Callers that need
a specific focus should call `maki.task.focus()` afterwards.

**Parameters:**

- `{id}` (`string`) Task id, as returned by `list()`.

**Returns:** (`boolean|nil`, `string|nil`) true on success, or nil and an error.

**Example:**

```lua
local _, err = maki.task.remove("toolu_01")
```


## maki.text {#maki-text}

Text transformation utilities.

Helper functions for converting between text formats.

```lua
local md = maki.text.html_to_markdown(html)
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
Call this to confirm a language grammar is available. Throws if {lang} is unknown.
Custom grammar paths are not yet supported.

**Parameters:**

- `{lang}` (`string`) Language name, e.g. `"rust"`.
- `{opts?}` (`table?`) Options table (the `path` key is not yet supported).

**Example:**

```lua
maki.treesitter.language.add("lua")
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
maki.treesitter.language.register("typescript", { "ts", "tsx" })
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
local lang = maki.treesitter.language.get_lang("tsx")
if lang then print(lang) end -- "typescript"
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
local fts = maki.treesitter.language.get_filetypes("typescript")
-- { "ts", "tsx" }
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
`nvim_create_buf(false, true)`.

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

Looks up a semantic color from the current theme. Use this to keep
your plugin's colors consistent with the rest of the UI.

**Parameters:**

- `{name}` (`string`) Semantic color name, e.g. "accent" or "background".

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
  - `prefix` (`string`) prepend to the source before highlighting (affects token context). Default "".

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

### `maki.ui.flash()` {#maki-ui-flash}

```lua
maki.ui.flash({msg})
```

Shows a brief message in the status bar. The message disappears
after a short time. Good for confirming an action like "copied!"
or showing a transient warning.

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

Runs a built-in UI action by name, exactly as its default keybinding
would. Handy when a default key never reaches maki because tmux or
your terminal grabs it first: bind a new key with `maki.keymap.set`
and call this from it.

Valid names: `"file_picker"`, `"search"`, `"help"`,
`"plan_toggle"`, `"plan_editor"`, `"edit_input"`, `"pop_queue"`,
`"prev_chat"`, `"next_chat"`, `"model_picker"`.

For slash commands rather than keybound actions, see
`maki.api.run_command`.

**Parameters:**

- `{name}` (`string`) Action name, e.g. `"file_picker"`.

**Returns:** (`boolean|nil`, `string|nil`) `true` on success, or nil and an error message for an unknown name.

**Example:**

```lua
-- Open the built-in file picker with Ctrl+Q instead of Ctrl+S:
maki.keymap.set("n", "<C-q>", function()
  maki.ui.action("file_picker")
end)
```

---

### `maki.ui.open_editor()` {#maki-ui-open_editor}

```lua
maki.ui.open_editor({path})
```

Opens {path} in the user's `$EDITOR` (e.g. vim, nano) and waits for
it to close. This suspends the TUI while the editor is running.
Returns the editor's exit code so you can check if the user saved.

**Parameters:**

- `{path}` (`string`) File to open.

**Returns:** (`integer`) Editor exit code, or -1 if the action could not be dispatched.

**Example:**

```lua
local code = maki.ui.open_editor("/tmp/scratch.lua")
if code == 0 then
  maki.ui.flash("File saved")
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

**Parameters:**

- `{buf}` ([`Buf`](#maki-ui-Buf)) Buffer to display.
- `{opts}` (`table`) Float configuration. Fields:
  - `width` (`integer|string`) window width. Integer for absolute columns; "N%" for percent of terminal width. Default "60%".
  - `height` (`integer|string`) window height. Integer for absolute rows; "N%" for percent of terminal height. Default "70%".
  - `row` (`integer?`) row offset from the anchor corner. Negative values move up.
  - `col` (`integer?`) column offset from the anchor corner.
  - `anchor` (`string`) corner the (row, col) offset is relative to. One of "NW" (default), "NE", "SW", "SE".
  - `border` (`string`) border style. One of "rounded" (default), "single", "double", "none".
  - `title` (`string`) text shown in the top border. Default "".
  - `title_pos` (`string`) title alignment. One of "left" (default), "center", "right".
  - `footer` (`table`) key-hint pairs shown in the bottom border. Each entry is {key, label}.
  - `zindex` (`integer`) stacking order. Default 50.
  - `cursor_line` (`boolean`) highlight the focused row. Default false.
  - `reserved_top` (`integer`) rows reserved at the top of the content area. Default 0.
  - `reserved_bottom` (`integer`) rows reserved at the bottom of the content area. Default 0.
  - `split` (`string`) dock the window to an edge instead of floating. One of "above", "below", "left", "right", "panel", or "" (floating, default).
  - `order` (`integer`) paint order among split windows at the same edge. Default 50.
  - `focus` (`boolean`) whether the window takes keyboard focus on open. Default true.
  - `visible` (`boolean`) whether the window is initially visible. Default true.
  - `needs_input` (`boolean`) whether the window means the session needs user input. Default false.
  - `stack` (`boolean`) offset the window past the other stacked windows sharing its anchor, in open order, with a one row gap. Closing one moves the rest up. Floating windows only. Default false.

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
maki.ui.set_status_hint({spans})
```

Shows key hints in the status bar for your plugin. Each hint is a {key, label} pair. Pass nil to clear your plugin's hints. Only your own hints are affected, other plugins keep theirs.

**Parameters:**

- `{spans}` (`table|nil`) Sequence of {key, label} pairs, e.g. `{{"q", "quit"}, {"j", "down"}}`. Pass nil to remove the plugin's hints.

**Example:**

```lua
maki.ui.set_status_hint({ {"q", "quit"}, {"j", "down"} })
-- later, clear them:
maki.ui.set_status_hint(nil)
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
- `{type="key", key}` -- keypress. Key is a string like "q", "j", or "esc".
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
  if ev.type == "key" and ev.key == "j" then
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
  - `anchor` (`string`) corner origin, "NW", "NE", "SW", or "SE".
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
resolve, so they follow the user's palette.

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
  local bg = maki.ui.theme_color("background")
  return bg and M.lerp(color, bg, factor)
end

return M
```

### `require("maki.dir_listing")`

```lua
-- Shared directory listing for index and list plugins.
-- Lists entries, filters instruction files, sorts dirs before files, and
-- renders the listing so every caller shows a directory the same way.
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
-- decides. {items} is a list of strings or { label, detail? } tables. {opts}:
-- title, footer, cursor (initial index), submit_keys (extra submit keys
-- besides enter), action_keys (keys that close the picker and report
-- themselves, like { "R" } for a refresh binding. Use uppercase keys, since
-- lowercase ones keep feeding the filter). Returns
-- { type = "choice"|"delete", index }, { type = "key", key, index? } or
-- { type = "close" }.
function ListPicker.open(items, opts)
ListPicker.split_words = split_words
ListPicker.matches = matches
ListPicker.highlight_spans = highlight_spans
```

### `require("maki.output_limits")`

```lua
-- Shared per-tool output limit options, so the tools that support them
-- cannot drift apart.

local DEFAULT_MAX_OUTPUT_LINES = 2000
local DEFAULT_MAX_OUTPUT_BYTES = 50 * 1024
local DEFAULT_MAX_LINE_BYTES = 500

local M = {}

M.DEFAULT_MAX_LINE_BYTES = DEFAULT_MAX_LINE_BYTES
M.specs = {
  max_output_lines = { type = "integer", desc = "Override `agent.max_output_lines` for this tool." },
  max_output_bytes = { type = "integer", desc = "Override `agent.max_output_bytes` for this tool." },
}

function M.extend(spec)
  for name, s in pairs(M.specs) do
    spec[name] = s
  end
  return spec
end

--- Returns max_lines, max_bytes: tool override when set, agent-wide otherwise.
function M.resolve(opts, ctx)
  return opts.max_output_lines or ctx:config("max_output_lines", DEFAULT_MAX_OUTPUT_LINES),
    opts.max_output_bytes or ctx:config("max_output_bytes", DEFAULT_MAX_OUTPUT_BYTES)
end

return M
```

### `require("maki.partial")`

```lua
-- When a tool is cut short, it still hands back what it printed. The marker
-- tells the model that output is real but unfinished. One home for the
-- wording and the painting, so every tool says it the same way.

--- Close {view} on the marker and build the tool reply. {out} is everything
--- the tool streamed, already truncated; empty means the view still shows a
--- placeholder to drop. {reason} is a cancel-hook reason ("cancelled" |
--- "timeout").
function M.cut(view, out, reason, timeout_secs)
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
-- R.CHANGED. Parent dispatchers must filter their own keys (esc, ctrl+c,
-- submit keys, etc.) BEFORE forwarding, because `handle_key` claims any key
-- it can interpret. `ctrl+a` is bound to move-home; if a parent wants it for
-- "select all" it must intercept first.
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
function ToolView:set_highlight(content, ext)

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
-- "markdown"`); {opts.width} is the wrap width. Errors stay plain, as they do
-- live.
function ToolView.restore_markdown(output, is_error, opts)
```

### `require("maki.truncate")`

```lua
local function truncate(text, max_lines, max_bytes)
  if #text <= max_bytes then
    local n = 0
    for _ in text:gmatch("\n") do
      n = n + 1
    end
    if n + 1 <= max_lines then
      return text
    end
  end
  local out = {}
  local bytes = 0
  local lines = 0
  for line in text:gmatch("([^\n]*)\n?") do
    lines = lines + 1
    if lines > max_lines then
      break
    end
    local new_bytes = bytes + #line + 1
    if new_bytes > max_bytes then
      break
    end
    out[#out + 1] = line
    bytes = new_bytes
  end
  local result = table.concat(out, "\n")
  if #result < #text then
    result = result .. "\n\n[truncated " .. (#text - #result) .. " bytes]"
  end
  return result
end

return truncate
```

