+++
title = "Hooks"
weight = 13
[extra]
group = "Reference"
+++

# Hooks

Maki has two ways for Lua to react to what the agent does.

| You want to | Use |
| --- | --- |
| Know that something happened | [Autocmds](/docs/lua-api/#maki-api-create_autocmd) |
| Change what happens, or stop it | Slots |

Autocmds are notifications. Many plugins can listen to one event, they run in no
particular order, and what they return is ignored. Slots are a chain: each layer
gets the value, decides, and passes it down by calling `prev`. The last layer
registered runs first.

Both come from Neovim. An autocmd matches `nvim_create_autocmd`, and a slot
plays the role `vim.ui.select` plays there, with the wrapping made explicit so
two plugins can layer the same point without capturing each other's function.

## Tool slots

Every tool has two slots that maki fires itself, so the tool's author does not
have to add a hook point. Both fire from the single function every tool call
passes through, so builtins, MCP tools, and ACP client tools behave alike:

| Slot | Fires | Gets |
| --- | --- | --- |
| `tool.<name>.input` | before the input is parsed or checked against your permission rules | the call the model wrote |
| `tool.<name>.output` | on the result of a call that ran, including a failure or a permission refusal | `{ text, is_error }` |

A call an input layer stopped never reaches the output slot: the reason came
from a layer, so there is nothing left to filter. A name that resolves to no
tool fires neither slot.

Layers take `function(prev, value, ctx)` and answer in one of three ways:

- return a table: it replaces the value for the rest of the call
- return nothing: the value is left alone
- return `nil, reason`: the call is stopped and the model reads `reason`

Stopping means something different per stage. On `input` the tool never runs and
`reason` becomes the tool result, marked as an error. On `output` the work is
already done, so `reason` only replaces the text the model reads.

`ctx` carries `tool`, `tool_id`, `session_id`, and `origin`. Origin is `"model"`
for a call the model made and `"nested"` for one made on its behalf by `batch`,
`code_execution`, or a plugin calling `maki.agent.call_tool`. Nested calls have
no id of their own, so their `tool_id` is empty. A subagent runs its own model,
so its calls arrive as `"model"` under the subagent's `session_id`.

### Rewriting a command

The model reaches for `grep -r` even when `rg` is installed. Denying costs a
model round trip every time it happens. A rewrite fixes the command in place:

```lua
maki.api.set_slot("tool.bash.input", function(prev, input, ctx)
  local rewritten = input.command:gsub("^grep %-r ", "rg ")
  if rewritten == input.command then
    return
  end
  input.command = rewritten
  return prev(input, ctx)
end)
```

`rg` and `grep -r` do not search the same files: `rg` skips what `.gitignore`
lists, hidden files, and binaries. That is why maki does not do this for you.

### Blocking a command

When there is no good rewrite, stop the call and say why. The reason reaches
the model as the tool result:

```lua
maki.api.set_slot("tool.bash.input", function(prev, input, ctx)
  if input.command:find("git push %-%-force") then
    return nil, "Force pushing is not allowed here. Open a PR instead."
  end
  return prev(input, ctx)
end)
```

### Trimming output

An output layer runs before the output becomes part of the conversation, so what
it drops is never paid for again:

```lua
local MAX = 200

maki.api.set_slot("tool.bash.output", function(prev, out, ctx)
  local lines = {}
  for line in out.text:gmatch("[^\n]+") do
    if not line:match("^%s*Compiling ") then
      table.insert(lines, line)
    end
  end
  out.text = table.concat(lines, "\n", 1, math.min(#lines, MAX))
  return prev(out, ctx)
end)
```

A replacement table has to carry `text`. Without it the output is left alone and
the reason is logged. Set `is_error` to turn a success into a failure, or a
failure into a success.

An output slot fires only when the text is the whole output. Tools the UI renders
from fields, like `read` or `edit`, are excluded, because prose edited underneath
would disagree with the display. So are tools whose result carries structured
state saved with the session, like `batch` or `question`. That state is what gets
re-rendered on restore, so a value redacted in the text would come back after a
restart.

## Rules

**Permissions judge what runs.** An input layer runs before the schema check and
before rules are resolved, so a layer cannot turn `allow bash: git status` into
something else. The prompt you see names the rewritten call.

**A layer borrows the tool's capability.** Wrapping `tool.bash.input` decides
what bash runs, so the plugin holding that layer needs `run`, the capability the
bash tool declares. A layer from a plugin without it is skipped and the rest of
the chain still runs. The check happens per call, so a missing grant shows up in
debug logs rather than as a warning at load.

| Tool | A layer needs |
| --- | --- |
| declares a permission, like `bash` | that permission |
| declares none: `read`, `batch`, MCP tools, ACP client tools, `tool_search` | every permission |

Declaring no capability does not mean a tool uses none. `batch`,
`code_execution` and `task` declare nothing while invoking any other tool, so
reading undeclared as free would hand a plugin everything. Undeclared costs the
maximum instead. See [plugin permissions](/docs/lua-api/#plugin-permissions).

**A layer may wait, within a window.** Chains are async, so a layer can read a
file or run a job before it decides. It runs inside the call it is filtering, so
cancelling the call cancels the layer too. Each stage gets whatever the call has
left of its own deadline, capped at 60 seconds. A layer still running when the
window closes is dropped, and the call proceeds as if that layer had passed the
value along.

Cancellation lands differently on the two stages. An input layer cut short stops
the call, because nothing has run yet and nobody is left to read a result. An
output layer cut short leaves the output as it found it, since the work is
already done.

**A broken layer is skipped.** If a layer throws, the chain continues as if it
had passed the value along, and the error is logged with the plugin name.

**Order is registration order.** The last layer registered is the outermost one
and sees the value first. Package load order decides this, so avoid writing two
layers that only work in one order.

**`prev` is single use.** Calling it twice throws. Everything below a layer runs
once per call.

**History keeps the call the model wrote.** The tool header, the permission
prompt, and the tool result show what ran. If a rewrite changes what the call
means, tell the model by appending a line in the output layer.

**Idle slots cost nothing.** A tool with no layers never crosses into Lua, and a
chain that hands back the value it was given leaves the original untouched.

**JSON null arrives as `nil`.** A Lua table cannot hold a null, so a null field
and a field that was never there look the same inside a layer. Maki carries
nulls across for you, which is what makes an untouched value a true no-op. The
cost: you cannot delete a field whose value is null, because maki cannot tell
that apart from leaving it alone. Set it to another value, or deny the call.

## Wrapping every tool

Slot names are per tool, so a layer on `tool.bash.input` costs nothing when
`read` is called. To cover all of them, loop over the registry:

```lua
local function redact(prev, out, ctx)
  out.text = out.text:gsub("sk%-%w+", "[redacted]")
  return prev(out, ctx)
end

for _, tool in ipairs(maki.api.get_tools()) do
  maki.api.set_slot("tool." .. tool.name .. ".output", redact)
end
```

Most builtins declare no capability, so this loop only does anything for a plugin
granted every permission. Run it from a plugin without them and each layer is
skipped when its tool is called.

This sees the tools registered so far, so run it from `init.lua`, which loads
after the builtin plugins. It also misses MCP tools, which arrive when their
server connects. Naming one slot directly has no such ordering rule: `set_slot`
accepts a name before anything registers it.

## Plugin slots

A plugin can define an extension point of its own with
[`declare_slot`](/docs/lua-api/#maki-api-declare_slot). The declaring plugin
owns the name and supplies the default, and anyone can wrap it with `set_slot`:

```lua
-- owner
local render = maki.api.declare_slot("myplugin.render", function(text)
  return text:upper()
end)

-- anyone
maki.api.set_slot("myplugin.render", function(prev, text)
  return "[" .. prev(text) .. "]"
end)

-- render("hi") now returns "[HI]"
```

Names starting with `tool.` are reserved for maki, which fires them at points
whose ordering it guarantees.

Use `maki.api.get_slots()` to see who owns and who wraps each slot.

## Limits

A layer has no agent context, so `maki.agent.call_tool` and
`maki.agent.session` are out of reach inside one. Read files, run jobs, and
decide from those.
