+++
title = "Lua Packages"
weight = 11
[extra]
group = "Reference"
+++

# Lua packages

A Lua package lets you add tools, commands, keybindings, and event handlers
without copying its code into `init.lua`. Maki can load a package that you put
on disk, or it can install one from a Git repository and lock it to one commit.

A package directory holds sorted `plugin/*.lua` entry files, modules at
`lua/<module>.lua` or `lua/<module>/init.lua`, and a `plugin.toml` manifest.
The entry files share one environment and use the API the
[plugin guide](/docs/plugins/) describes.

## Install from Git

Declare managed packages in the global `init.lua`, normally
`~/.config/maki/init.lua`:

```lua
maki.pack.add({
  "https://github.com/example/maki-goal",
  {
    src = "https://github.com/example/maki-review",
    version = "v1.2.0",
  },
})
```

Each entry is a source string, or a table with `src`, `version`, `name`, and
`data`. Maki derives the directory and owner name from `src`, and `name`
overrides it when two sources end in the same repository name.

Maki shows all new packages in one install prompt, on the terminal, before the
UI starts. It writes the selected Git commit to `pack-lock.json` in your config
directory. Commit this file if you want another machine to install the same
revisions.

Every project shares one lockfile and package directory, so a project
`.maki/init.lua` cannot add packages. It can still read state with
`maki.pack.get` and activate a package with `maki.packadd`.

Maki refuses a package name that matches a builtin plugin or a package you
placed by hand, and reports the conflict at startup.

Set `confirm = false` only when the package source is already trusted and Maki
must run without a terminal:

```lua
maki.pack.add({ "https://github.com/example/maki-goal" }, {
  confirm = false,
})
```

This option skips the install prompt. It does not approve package permissions.
Maki rejects an HTTP source carrying a username, password, or token, since Git
and the lockfile would store it. Use a credential helper or an SSH agent.

Set `load = false` to install a package without loading it at startup. Set
`load` to a function when the package needs a custom entry point:

```lua
maki.pack.add({
  {
    src = "https://github.com/example/maki-review",
    data = { module = "review" },
  },
}, {
  load = function(package)
    require(package.spec.data.module).setup()
  end,
})
```

The function runs as the package owner. It receives the package `spec` and its
installed `path`. The `data` field can contain any Lua value.

See [`maki.pack.add`](/docs/lua-api/#maki-pack-add) and
[`maki.pack.get`](/docs/lua-api/#maki-pack-get) for the full signatures.

## Pinned revisions

A lockfile entry wins over `version`: once Maki records a commit, it installs
that commit everywhere, and a later `version` in `init.lua` changes nothing. To
move a package, delete its entry from `pack-lock.json` and
start Maki again. Maki resolves `version` and records the commit it picked.

A changed `src` makes the recorded revision meaningless, so Maki installs the
new source and records it. That is also a new trust decision, so the install
and permission prompts come back.

Removing a `maki.pack.add` entry stops the package from loading. Its lockfile
entry and its checkout stay on disk until you delete them.

## Update packages

Run `/packupdate` to update every installed package that the global config
still declares. Pass one package name to update only that package:

```text
/packupdate maki-review
```

Maki fetches the source and shows the old and proposed commits with requested
permission additions and removals before it changes `pack-lock.json`.
Declining the review changes no installed revision. Use `/packupdate!` to skip
the update review. It does not approve new permissions. Maki asks for that
approval separately when it loads the updated package.

Pass `++lockfile` to restore the commit already recorded in the lockfile
instead of resolving the declared version:

```text
/packupdate ++lockfile maki-review
```

The global `init.lua` can queue the same operations in Lua. Project config and
packages cannot update global package state.

```lua
maki.pack.update({ "maki-review" })
maki.pack.update({ "maki-review" }, {
  force = true,
  target = "lockfile",
})
```

`force = true` has the same review-bypass behavior as `/packupdate!`.

## Remove packages

First remove the package declaration from the global `init.lua` and reload.
Then remove the inactive package:

```text
/packdel maki-review
```

`/packdel ++all` removes every installed package that is no longer declared.
Maki refuses a package that is active in this process or another Maki process.
It removes the package approval only after the package files can be removed.

The global `init.lua` can also queue deletion:

```lua
maki.pack.del({ "maki-review" })
```

There is no force option for deletion.

## Package permissions

A managed package can ask for guarded APIs in `plugin.toml`:

```toml
[permissions]
fs_read = true
fs_write = true
net = true
run = true
env = true
```

The manifest states what the package wants, and Maki asks about new permissions
in a separate prompt. A package with no `plugin.toml` asks for nothing, and
every guarded call it makes fails. The
[permission list](/docs/lua-api/#plugin-permissions) covers what each name
gates.

An approval applies only to the same package name and source. Maki keeps
approvals in `<maki-data>/site/pack-approvals.json`, where `<maki-data>` is the
data directory from the
[directory layout](/docs/configuration/#directory-layout). Approvals describe
trust on this machine and must not be committed with `pack-lock.json`.

Only the interactive UI can ask. `--print`, SDK mode, the ACP server, and the
other subcommands never prompt, so a package waiting for a decision comes back
as a startup warning instead of loading.

## Install by hand

Clone a package into a Neovim-style package directory:

```text
<maki-data>/site/pack/<group>/start/<name>/
<maki-data>/site/pack/<group>/opt/<name>/
```

Pick any `<group>` name except `core`, which Maki reserves for its own installs
and never scans.

A `start` package loads at startup. An `opt` package stays installed until
something activates it.

Placing a package by hand means you trust its code the way you trust your own
`init.lua`. Maki grants it exactly the permissions its `plugin.toml` requests,
with no approval step.

## Activate an installed package

An `opt` directory, or a managed package declared with `load = false`, starts
with `maki.packadd`. Call it from `init.lua` or from another package:

```lua
maki.packadd("my-package")
```

Maki loads the named package after the calling Lua task returns. A package
that `maki.packadd` activates can activate another one in turn. Maki reports a
name that no installed package matches, and refuses a package that
`plugins.<name>.enabled = false` disabled.

## Managed checkouts

Maki restores a missing checkout only when the global config still declares the
package and its source matches the lockfile entry. Installs and approval writes
share one file lock, so two Maki processes cannot interleave them. The kernel
releases that lock when the process exits, so a crash leaves nothing to clean
up.

Each revision gets its own directory under
`<maki-data>/site/pack/core/<name>/<commit>/`. At startup Maki deletes stale
revisions, skipping any that a running process still holds.
