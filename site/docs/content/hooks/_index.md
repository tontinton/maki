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

Name the tool `*` to wrap every tool. See
[wrapping every tool](#wrapping-every-tool).

A call an input layer stopped never reaches the output slot: the reason came
from a layer, so there is nothing left to filter. A name that resolves to no
tool fires neither slot.

Layers take `function(prev, value, ctx)` and answer in one of these ways:

- return a table: it replaces the value for the rest of the call
- return nothing: the value is left alone
- return `nil, reason`: the call is stopped and the model reads `reason`
- on `input` only, return `{ ask = reason }` as the second value: the user
  must approve the call. See [asking the user](#asking-the-user).

A layer that answers without calling `prev` ends the chain, and the layers
below it never run. To pass a value on untouched, `return prev(value, ctx)`.

Stopping means something different per stage. On `input` the tool never runs and
`reason` becomes the tool result, marked as an error. On `output` the work is
already done, so `reason` only replaces the text the model reads.

| `ctx` field | Meaning |
| --- | --- |
| `tool` | tool name |
| `tool_id` | id of the call, empty for nested calls |
| `tool_kind` | kind the tool declares, like `read`, `edit`, or `execute`. `nil` if it declares none. |
| `origin` | `"model"`, or `"nested"` when `batch`, `code_execution`, or `maki.agent.call_tool` made the call |
| `session_id` | session of the call. A subagent's calls arrive as `"model"` under its own session. |
| `deadline_ms` | milliseconds left before the layer is dropped |
| `input` | `output` only: the input the call ran with, after every input layer |

### Rewriting a command

The model reaches for `grep -r` even when `rg` is installed. Denying costs a
model round trip every time it happens. A rewrite fixes the command in place:

```lua
maki.api.set_slot("tool.bash.input", function(prev, input, ctx)
  input.command = input.command:gsub("^grep %-r ", "rg ")
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

### Asking the user

Some calls are risky in a way only a plugin can spot. Return
`{ ask = reason }` as the second value to raise the permission prompt, with
`reason` shown in it:

```lua
maki.api.set_slot("tool.bash.input", function(prev, input, ctx)
  if input.command:find("terraform apply", 1, true) then
    return nil, { ask = "This changes live infrastructure." }
  end
  return prev(input, ctx)
end)
```

The prompt appears even when an allow rule or yolo mode would let the call
through. A deny rule still wins. To ask about a rewritten call, return it
first: `return input, { ask = reason }`.

Each driver answers this prompt like any other. `maki -p` denies it, and an SDK
client in `bypassPermissions` mode allows it without showing it.

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
layers that only work in one order. The exception is `tool.*`: its layers
always sit outside the tool's own layers.

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

`tool.*.input` and `tool.*.output` fire for every tool, including MCP and ACP
client tools that register later:

```lua
maki.api.set_slot("tool.*.output", function(prev, out, ctx)
  out.text = out.text:gsub("sk%-%w+", "[redacted]")
  return prev(out, ctx)
end)
```

`tool.*` layers wrap the tool's own layers, whatever the load order, so a
`tool.bash.input` layer sees the value after them.

A bare `return` in a `tool.*` layer skips the tool's own layers too, which can
switch off another plugin's block. Hand the tools you do not care about to
`prev` instead.

Each call charges the layer what that tool charges. A plugin granted only `run`
has its `tool.*` layer run for `bash` and skipped for `read`, which declares no
capability.

## Agent slots

The agent loop fires four slots of its own:

| Slot | Fires | Gets |
| --- | --- | --- |
| `agent.user_message` | before the model sees a user message | `{ text, images, source }` |
| `agent.stop` | when the model ends its turn without calling a tool | `{ reason, last_message, num_turns }` |
| `agent.compact.before` | before a compaction starts | `{ reason, context_size, usable, request_instructions }` |
| `agent.compact.prepare` | before the transcript goes to the summarizer | `{ results, budget, collapse }` |

They follow the tool slot contract. `ctx` carries `session_id`, `task_id` (set
inside a subagent), `model`, `context_size`, `context_window`, and
`deadline_ms`.

A layer on any of them needs every permission, because it steers what the agent
does next. The chain gets 30 seconds, then the loop carries on without it.

### agent.user_message

`images` is the number of attached images. `source` is `"tui"`, `"acp"`,
`"headless"` (`maki -p` and sdk mode), or `"plugin"`. Messages sent while the
agent is working fire it too.

Return `{ text = ... }` to rewrite the message. History keeps the new text, and
the transcript marks it as rewritten. Return `nil, reason` to drop it: the model
never sees it, the user sees `reason`, and `TurnEnd` reports `"dropped"`. A
rewrite with no text and no images also drops it.

```lua
maki.api.set_slot("agent.user_message", function(prev, msg, ctx)
  if msg.text:find("AKIA%u%u%u%u") then
    return nil, "That message holds an AWS key. Remove it and send again."
  end
  return prev(msg, ctx)
end)
```

### agent.stop

`reason` is `"finished"` or `"max_tokens"`, and `last_message` is the text of
the final answer. Return `{ continue = text }` to send `text` as the next
message and keep going:

```lua
maki.api.set_slot("agent.stop", function(prev, stop, ctx)
  local todo = maki.fs.read("TODO.md")
  if todo and todo:find("%- %[ %]") then
    return { continue = "TODO.md still has open items. Keep going." }
  end
  return prev(stop, ctx)
end)
```

A run continues at most 3 times in a row, so a layer that always answers
`continue` cannot bill forever. The count resets on the next user message.

### agent.compact.before

`reason` is `"auto"` (the context crossed its threshold), `"overflow"` (the
provider rejected the prompt as too long), or `"manual"` (`/compact`). `usable`
is the token budget `context_size` is measured against. `request_instructions`
is what the user typed after `/compact`.

Return a table with any of:

- `skip = true`: skip this compaction, same as `nil, reason`. An overflow
  cannot be skipped, because the run cannot go on without compacting.
- `instructions`: appended to the summary prompt, after the configured and
  requested ones.
- `continue`: appended to the message the agent resumes with.

### agent.compact.prepare

Before summarizing, maki replaces older tool results with a placeholder so the
summarizer reads the recent ones in full. This slot picks which ones.

`results` lists every tool result, oldest first, as
`{ index, tool, input, bytes, text }`, with `text` cut to 4 KB. `index` is the
position in `results`. `budget` is how many bytes of results maki keeps in full,
and `collapse` holds the indexes it picked. Set `collapse` to replace that pick.
This layer keeps every `todo_write` result:

```lua
maki.api.set_slot("agent.compact.prepare", function(prev, prep, ctx)
  local collapse = {}
  for _, i in ipairs(prep.collapse) do
    if prep.results[i].tool ~= "todo_write" then
      table.insert(collapse, i)
    end
  end
  prep.collapse = collapse
  return prev(prep, ctx)
end)
```

Maki applies the pick itself, so a layer cannot split a tool call from its
result.

## Completion sources

The bundled completion plugin offers files after `@`. Other plugins add entries
through the `completion.sources` slot, which is free to layer. Append your
sources and pass the list on:

```lua
maki.api.set_slot("completion.sources", function(prev, sources)
  table.insert(sources, {
    trigger = "#",
    name = "issues",
    complete = function(query, ctx)
      local out = maki.fn.jobwait(maki.fn.jobstart({ "gh", "issue", "list", "--search", query }))
      local items = {}
      for num, title in out.stdout:gmatch("(%d+)%s+%S+%s+([^\t]+)") do
        table.insert(items, { text = "#" .. num, label = "#" .. num .. " " .. title })
      end
      return items
    end,
  })
  return prev(sources)
end)
```

| Field | Meaning |
| --- | --- |
| `trigger` | one punctuation character. Typed at the start of a word, it opens the popup. Sources on `@` add rows next to the files. |
| `name` | unique across sources. A second source with the same name is ignored. |
| `complete` | `function(query, ctx)` returning items. `query` is the text after the trigger. |

An item is `{ text, label, score }`. `text` replaces the mention, and `label` is
what the row shows (default `text`). Rows sort by `score`, highest first. File
rows score `0` and win ties, so use a positive score to rank above them.

On each keystroke, maki asks every source for that trigger in parallel and
shows rows as they arrive. A newer keystroke replaces the query, and a source
that has not answered within one second is left out. `ctx.cancelled()` turns
true at that point, so a long loop can stop early. `ctx` also carries
`trigger`, `session_id`, and `cwd`. A source that throws or returns `nil, err`
shows no rows, and the error is logged.

`complete` runs with the permissions of the plugin that wrote it, so the example
needs `run`.

## Plugin slots

A plugin can define an extension point of its own with
[`declare_slot`](/docs/lua-api/#maki-api-declare_slot). The declaring plugin
owns the name and supplies the default, and wraps its own slot for free. A
layer from any other plugin steers a chain the owner's callers trust, so it
pays, and the owner sets the price, because the owner is the only one who knows
what its default does with the arguments:

```lua
-- owner: layering this only rewrites text, so say so
local render = maki.api.declare_slot("myplugin.render", function(text)
  return text:upper()
end, { capability = {} })

-- any other plugin, granted nothing
maki.api.set_slot("myplugin.render", function(prev, text)
  return "[" .. prev(text) .. "]"
end)

-- render("hi") now returns "[HI]"
```

`capability` is a list of permission names, and a layer needs all of them at
once. An empty list is free for anyone. Leaving `capability` out charges every
permission, which is what a tool declaring no capability charges: a slot that
named no price has not promised its default exercises none, only that nobody
asked. You can only name permissions your own plugin holds.

The rule is the one the `tool.*` slots use, and it is read the same way: every
call re-reads what each layer's plugin holds, so a layer registered by a plugin
without the grant is skipped and the call carries on, and a reload that narrows
a plugin's permissions costs it the layer on the next call.

A slot name belongs to the plugin that declared it for as long as maki runs.
Unloading the owner stops the chain firing, but does not free the name: nobody
else can take it over, or re-declare it at a cheaper price and inherit the
layers that trusted the old one.

Names starting with `tool.`, `ui.`, and `agent.` are reserved for maki, which
fires them at points whose ordering it guarantees.

Use `maki.api.get_slots()` to see who owns and who wraps each slot.

## Limits

A layer has no agent context, so `maki.agent.call_tool` and
`maki.agent.session` are out of reach inside one. Read files, run jobs, and
decide from those.
