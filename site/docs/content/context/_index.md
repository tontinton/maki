+++
title = "Context"
weight = 31
[extra]
group = "Concepts"
+++

# Context

Everything the model knows about your project passes through one context window, and every token in it costs money and attention. This page covers what Maki puts there, when, and where you should put things so they land well.

## What loads when

```
session start (paid every request)   on demand (paid when used)
──────────────────────────────────   ─────────────────────────────────
system prompt                        file contents   read / index / grep
tool definitions                     skill bodies    skill tool
instruction files (AGENTS.md, ...)   memory notes    memory tool
memory tag names                     subdir rules    first read there
skill names + descriptions           MCP tool defs   tool_search
```

The left column is the fixed overhead of every single request, so Maki keeps it small on purpose: a skill contributes one description line, memories one list of tags, a big MCP server one search tool. The bodies stay on disk until the agent asks.

## Instruction files

At session start Maki walks from the project git root down to the working directory (no `.git` root, only the cwd). In each directory it loads **one** project instruction file, first match wins:

| Order | File |
|------|------|
| 1 | `AGENTS.md` |
| 2 | `CLAUDE.md` |
| 3 | `.github/copilot-instructions.md` |
| 4 | `COPILOT.md` |
| 5 | `.cursorrules` |
| 6 | `.windsurfrules` |
| 7 | `.clinerules` |
| 8 | `CONVENTIONS.md` |
| 9 | `GEMINI.md` |
| 10 | `CODING_AGENT.md` |

After the match it always loads `AGENTS.local.md` from the same directory if present: that one is yours, keep it gitignored. Closer directories win on conflicts. Finally one global `~/.config/maki/AGENTS.md` for preferences that follow you across projects.

```
~/repo/AGENTS.md           loaded (root)
~/repo/AGENTS.local.md     loaded (yours, gitignored)
~/repo/api/CLAUDE.md       loaded when cwd is ~/repo/api, wins over root
~/repo/web/AGENTS.md       not loaded yet...
~/.config/maki/AGENTS.md   loaded (global)
```

That `web/AGENTS.md` is not dead weight. The first time the agent `read`s a file under a subdirectory whose instruction file was never loaded, Maki pulls it in. Monorepo rules live next to the code they govern and cost nothing until someone works there.

Put coding conventions, repo quirks, and off-limits directories in these files. Keep them short; the next section explains why.

## Four places to put knowledge

All four end up in context, but at different times and prices:

| | Loaded | Costs | Good for |
|---|--------|-------|----------|
| `AGENTS.md` | every session | every request | short rules: conventions, build commands, no-go areas |
| [Skills](/docs/skills/) | when the agent picks one | a description line until then | long playbooks: release process, plugin authoring |
| Memory | when the agent recalls a tag | tag names until then | gotchas the agent learns while working |
| [Commands](/docs/commands/) | when you type `/name` | nothing until invoked | prompts you keep retyping |

Rule of thumb: when `AGENTS.md` grows past a screen, the new material probably wants to be a skill. `AGENTS.md` is a tax on every request; a skill is a tax only on the sessions that need it.

## Pointing at a file with `@`

Naming the file saves the agent a search, which costs a tool call and a few hundred tokens. Type `@` in the chat input to open a completion popup, ranked like the `Ctrl+S` file picker:

```
> explain @maki-ui/src/app/mo
                ╭─────────────────────────────────╮
                │ maki-ui/src/app/mod.rs          │
                │ maki-ui/src/app/model.rs        │
                ╰ ↑/↓ move Enter insert Esc close ╯
```

| Key | Action |
|-----|--------|
| `↓`, `Ctrl+N` | Next row |
| `↑`, `Ctrl+P` | Previous row |
| `Enter` | Insert the highlighted path |
| `Esc` | Close the popup |

The popup takes these keys only while it is open. Otherwise `↑` and `↓` still walk the input history and `Ctrl+P` still opens `/sessions`. The characters your query matched are highlighted the way the file picker highlights them. Keep typing to narrow the list. A space ends the mention, so an email address does not open the popup. Moving the caret out of the mention closes it. With no match, `Enter` closes the popup without sending. While the agent is working, the first `Esc` only closes the popup, and after that `Esc` stops the turn as usual.

The inserted path is plain text in your message. Nothing is attached or read until the agent calls `read`.

The plugin is off by default while its file index is tested on large repositories. Turn it on in `init.lua`:

```lua
maki.setup({
  plugins = {
    completion = { enabled = true },
  },
})
```

On a large repository the first index walk takes a moment. The popup shows `scanning…` and fills in when the walk finishes. If the walk has not reported back after a few seconds, it shows `no matches`. Other options are under `plugins.completion` in [configuration](/docs/configuration/).

## When the window fills

Long sessions eventually approach the model's context limit. Maki reserves a slice of the window (`agent.compaction_buffer`, default 20%) and before running out it summarizes the older turns and continues from the summary. `/compact` triggers it early, `/compact keep the repro steps` steers that one summary, `/usage` shows where the tokens went, and `agent.compaction_instructions` steers every summary.

Compaction replaces the older turns in the session's on-disk log with the summary. The dropped turns are not lost: before the rewrite, Maki parks the previous log at `sessions/archive/<session-id>/<n>.jsonl` in the [state directory](/docs/configuration/#directory-layout). It keeps the newest three per session, and at most 32 MB of them. The names count up, so the highest number is the newest.

An archive is a complete session file, so `jq` or an editor reads it as it is. To open one in Maki you have to put it back in place of the live log, which drops the session's current state, so move that out of the way first:

```sh
cd ~/.local/state/maki/sessions
mv <session-id>.jsonl <session-id>.jsonl.bak
cp archive/<session-id>/<n>.jsonl <session-id>.jsonl
maki -s <session-id>
```

`MAKI_DISABLE_AUTOCOMPACT=1` turns off the automatic compaction. A manual `/compact` still compacts.

Related: [Token Economy](/docs/token-economy/) for why all this frugality exists, [Configuration](/docs/configuration/) for the knobs.
