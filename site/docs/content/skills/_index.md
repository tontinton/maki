+++
title = "Skills"
weight = 20
[extra]
group = "Guides"
+++

# Skills

A skill is a short Markdown how-to that the agent loads only when it needs it. The `skill` tool shows the agent what is available, and when it picks one, the file drops into the conversation and the agent follows it.

Write one for anything you keep explaining: how you cut a release, how you write a maki plugin, how a PR should look in this repo. `AGENTS.md` is always in context and always costs tokens. A skill costs nothing until it is loaded, only its name and description sit in the tool list. So big skills are fine.

## Where skills live

A skill is a directory with a `SKILL.md` inside. Maki looks for them every time the `skill` tool runs (and once at startup, to build the list). When two skills share a name, the one found last wins and Maki logs both
paths. Directory entries are sorted by name before scanning:

1. The builtin `maki-plugin-dev` (if enabled)
2. `~/.config/maki/skills/` (Windows: `%APPDATA%\maki\skills\`)
3. `~/.claude/skills/`, `~/.config/opencode/skills/`, `~/.agents/skills/`
4. In your project, walking from the current directory up to the `.git` root, at each step: `.maki/skills/`, `.claude/skills/`, `.opencode/skills/`, `.agents/skills/`

So project skills beat personal ones, and a skill at the repo root beats one with the same name deeper down. The `.claude`, `.opencode` and `.agents` dirs are there so skills you already wrote for other agents keep working.

[Folder trust](/docs/folder-trust/) gates project skills in every supported
location. Untrusted projects contribute no skill names, descriptions, or
instructions. Trust the project and restart or use `--trust` for one run.
Personal and builtin skills remain available without project trust.

Only `SKILL.md` is read. If you want extra notes, put them in files next to it and link them from the body, like `./notes.md`.

## Writing one

Make a directory under `.maki/skills/` and put a `SKILL.md` in it:

```
.maki/skills/git-release/SKILL.md
```

```markdown
---
name: git-release
description: Cut a tagged release and open the changelog PR
---

## Steps

1. Read `CHANGELOG.md` and the commits since the last tag.
2. Propose a semver bump and a short release summary.
3. Only tag after the user confirms.
```

Skills follow the [Agent Skills specification](https://agentskills.io/specification).
YAML frontmatter must contain a non-empty `name` and `description`.
Descriptions can contain up to 1024 characters. An optional `compatibility`
string can contain up to 500 characters. Empty instruction bodies are allowed.

Use a name of 1 to 64 lowercase letters, digits, and single hyphens, with no
hyphen at either end. Match the directory name. Maki warns about name
violations and loads the skill when possible, as the client integration guide
recommends. Invalid YAML, missing required fields, and incorrect field types
skip that file and report its path and cause at startup and in the log.
Other skills remain available.

Before skipping invalid YAML, Maki retries plain text metadata containing
`: ` as quoted strings, such as `description: Use when: the user asks`.
Structured values and other YAML errors still fail validation.

The optional `license`, `compatibility`, `metadata`, and `allowed-tools`
fields are retained when the skill loads. `metadata` must map strings to
strings. `allowed-tools` is experimental and advisory in Maki. It does not
approve tool calls or override your permission rules.

## How it gets used

The `skill` tool is registered only when skills are available. Its name
parameter is restricted to the startup catalog. It lists every skill it found, the agent calls it with a name and gets the full file and its resource directory back. A wrong name errors and reprints the list so the model can pick again.

Use `/skill git-release` to load a skill directly. Add a request after its
name, such as `/skill git-release prepare the changelog`. Maki resolves and
loads the file before it sends the prompt to the model. Missing or invalid
skills report an error without starting a model request. You can also ask the
agent to use a skill or let it select one. The same `/skill name [request]`
syntax works in headless and ACP user messages.

Loaded skill instructions survive manual and automatic compaction. Maki
retains the latest successful load for each requested skill name from the
current transcript. Reset and rewind follow the same history as the rest of
the session. Extra user requests are summarized normally.

## The builtin: maki-plugin-dev

Maki ships one skill, `maki-plugin-dev`. It teaches the agent how to write maki Lua plugins, and on load it writes the full Lua API reference to a file in the state dir, so the agent can read it in pieces instead of swallowing it whole. It carries the same guide you can read in [Plugins](/docs/plugins/), so "write me a plugin that ..." is usually enough. Turn it off if you never write plugins:

```lua
-- ~/.config/maki/init.lua
maki.setup({
    plugins = {
        skill = { plugin_dev = false },
    },
})
```
