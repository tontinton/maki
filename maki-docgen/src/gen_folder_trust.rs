//! Ordinary prose, split into pieces around the two parts that are filled in.

use std::fmt::Write;

use maki_config::GatedFile;

const INTRO: &str = r#"+++
title = "Folder Trust"
weight = 7
[extra]
group = "Reference"
+++

# Folder Trust

A project `.maki` directory can run code on your machine before you type
anything. Maki loads none of it until you trust the folder."#;

const BODY: &str = r#"The first interactive start in an untrusted project draws a card before the
main UI opens, listing the gated files it found. It takes three answers:

| Answer | Effect |
|--------|--------|
| Trust | Project config loads this run and every later one. |
| Not now | Restricted this run, asked again next start. |
| Never | Restricted, and not asked again. |

"Not now" is preselected, so Enter and Escape are both safe. `t` or `y` answers
Trust, `n` answers Not now, and "Never" needs the arrow keys and Enter. Ctrl-C
exits Maki. A project that ships no gated file is never asked about.

One answer covers one project root: the active Git checkout, or the working
directory outside Git. Linked worktrees answer for themselves. Starting Maki in
your home directory loads no project configuration, because `~/.maki` there is
your global configuration.

## What Trust Does Not Cover

Trust gates code. Text that a project puts into the prompt loads at any trust
level:

- `AGENTS.md` and the other instruction files
- Commands under `.maki/commands` and `.claude/commands`
- Skills under `.maki/skills`, `.claude/skills`, `.opencode/skills` and
  `.agents/skills`

A repository can still steer the agent through what the model reads, so trust
is not a sandbox. What limits the agent on each tool call is
[permissions](/docs/permissions/), at every trust level.

The `deny` scopes in `.maki/permissions.toml` apply without trust. Its `allow`
scopes and any `default` it sets are dropped, so a repository can only narrow
what the agent may do inside it.

## In an Untrusted Folder

Maki writes nothing into a folder you declined, and the status bar carries a
`[restricted]` indicator for the whole session. A folder with no `.maki` at all
shows no indicator.

The project answers in a [permission prompt](/docs/permissions/#permission-prompts)
still work and last until the session ends, labelled `Project (this session)`.
For an answer that outlives the session, use `A` or `D` to save it in your own
`~/.config/maki/permissions.toml`, or trust the folder.

## Managing Trust

```bash
maki trust add [PATH]        # asks before recording
maki trust add [PATH] --yes  # records a yes
maki trust remove [PATH]     # clears a yes or a no
maki trust list              # shows both kinds of decision
```

`PATH` defaults to the current directory. None of these commands start the Lua
host, so they are safe to run in a folder you have not read yet. Decisions are
stored outside the project and follow the checkout path.

Inside the TUI, `/trust` trusts the current folder and reloads plugins and
configuration. Typing it is the consent, so there is no second question. It
covers the gated files the folder had when the session started, so a kind the
project adds while Maki runs is asked about on the next start.

## Trust Policy

Answer in advance for paths you already trust:"#;

/// The configuration page prints this same block, so the two cannot drift.
pub const POLICY_EXAMPLE: &str = r#"```lua
maki.setup({
    trust = {
        paths = { "~/src/me/*", "/workspace" },
        prompt = false,
    },
})
```"#;

const REST: &str = r#"Maki reads `trust` from the global `~/.config/maki/init.lua` only. A project
`.maki/init.lua` that sets it has the table stripped and gets a warning, since a
project shipping one would be granting itself trust.

Patterns are matched against the project root. `*` stays inside one path
segment, `**` crosses segments, and `~` expands to your home directory.
`paths = { "**" }` trusts every folder. A match is recorded like any other yes,
so `maki trust list` shows it and `maki trust remove` clears it.

The policy answers only a folder that has no answer yet, so a recorded `Never`
stays a `Never` however the globs are written. Clear it with
`maki trust remove PATH`, or `/trust` inside the TUI.

`prompt = false` drops the card and leaves the folder restricted unless a
`paths` entry matches.

The policy applies to the TUI, `-p`, the SDK and ACP. The utility subcommands
(`maki index`, `maki models`, `maki prompt`, `maki mcp auth`) skip it, since a
grant there would record a decision you never saw.

## Containers and CI

Headless runs, the SDK, ACP, and utility subcommands never ask. An untrusted
folder is skipped, the skipped path is reported on standard error, and the run
continues on global configuration.

Pass `--trust` where the container is already the boundary you rely on:

```bash
maki --trust -p "run the test suite"
```

The flag loads the project configuration for that run and records no decision,
so a state directory shared by many containers collects no grants. Neither the
flag nor the policy has an environment variable, which would reach every child
process.

In an image you build yourself, a [trust policy](#trust-policy) in the global
`init.lua` covers every run without a flag on each command:

```lua
maki.setup({
    trust = { paths = { "/workspace/**" } },
})
```

## What a Yes Covers

Your yes covers the kinds of gated file the folder had that day. A project that
later adds a kind you were never asked about asks again.

Maki records the file names rather than their contents, so Lua that changes in a
later pull runs under the answer you already gave. Run `maki trust remove` when
that stops being what you want."#;

/// Rows come from the enum the gate itself walks, so a new gated kind cannot
/// ship undocumented.
fn gated_table() -> String {
    let mut table =
        String::from("| Gated file | What it can do |\n|------------|----------------|");
    for file in GatedFile::ALL {
        write!(table, "\n| `{file}` | {} |", file.describes()).unwrap();
    }
    table
}

pub fn generate() -> String {
    let table = gated_table();
    format!("{INTRO}\n\n{table}\n\n{BODY}\n\n{POLICY_EXAMPLE}\n\n{REST}\n")
}
