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

A skill is a directory with a `SKILL.md` inside. Maki builds the list of skills once at startup, so a skill added during a session needs `/reload`. When two skills share a name, the one found last wins and Maki logs both paths:

1. The builtin `maki-plugin-dev` (if enabled)
2. `~/.config/maki/skills/` (Windows: `%APPDATA%\maki\skills\`)
3. `~/.claude/skills/`, `~/.config/opencode/skills/`, `~/.agents/skills/`
4. In your project, walking from the current directory up to the `.git` root, at each step: `.maki/skills/`, `.claude/skills/`, `.opencode/skills/`, `.agents/skills/`

So project skills beat personal ones, and a skill at the repo root beats one with the same name deeper down. The `.claude`, `.opencode` and `.agents` dirs are there so skills you already wrote for other agents keep working.

Project skills load only in a [trusted folder](/docs/folder-trust/), and an
existing grant covers skills added later. Personal and builtin skills always
load.

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
The frontmatter needs a non-empty `name` and a `description` of up to 1024
characters. The name should be lowercase letters, digits, and single hyphens,
matching the directory; Maki warns about a mismatch but loads the skill.
A file with invalid YAML or a missing field is skipped, and its path and cause
are shown at startup. Unquoted values such as
`description: Use when: the user asks` are retried as quoted strings.

Optional fields such as `license` and `allowed-tools` are passed to the model
with the rest of the file. `allowed-tools` does not change Maki's permissions.

## How it gets used

The `skill` tool lists the skills found at startup, and is left out when there
are none. The agent calls it with one of those names and gets the full file and its resource directory back. A wrong name errors and reprints the list so the model can pick again.

Skills are not slash commands: typing `/git-release` does nothing unless you also add a [custom command](/docs/commands/#custom-commands). Ask the agent to use a skill, or let it pick one on its own.

Loaded skills survive compaction: Maki keeps the latest load of each skill
verbatim, newest first, in half the room left below the compaction threshold, up to
64 KiB and a tenth of the model's context window.

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
