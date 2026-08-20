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

## Install from Git

Declare managed packages in `init.lua`:

```lua
maki.pack.add({
  "https://github.com/example/maki-goal",
  {
    src = "https://github.com/example/maki-review",
    version = "v1.2.0",
    data = { style = "short" },
  },
})
```

Maki shows all new packages in one install prompt. It writes the selected Git
commit to `pack-lock.json` in your config directory. Commit this file if you
want another machine to install the same revisions.

The first `maki.pack.add` call restores missing lockfile entries in
alphabetical order before it installs new declarations.

Set `confirm = false` only when the package source is already trusted and Maki
must run without a terminal:

```lua
maki.pack.add({ "https://github.com/example/maki-goal" }, {
  confirm = false,
})
```

This option skips the install prompt. It does not approve package permissions.
Maki rejects an HTTP source that contains a username, password, or token because
Git and the lockfile would store it. Use a Git credential helper or an SSH agent
instead.

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

The file is a request, not an approval. Maki asks about new permissions in a
separate prompt. An approval applies only to the same package name and source.
An update that asks for another permission needs another approval, even when
you force the update.

Non-interactive modes do not read a prompt from standard input. They report the
package as unavailable until the required install and permission decisions are
made in the terminal UI.

## Load behavior

Packages load at startup by default. Use `load = false` to install a package
without loading it:

```lua
maki.pack.add({ "https://github.com/example/maki-goal" }, {
  load = false,
})
```

Load it later with:

```lua
maki.packadd("maki-goal")
```

Any plugin can call `maki.packadd`. Maki validates the package immediately and
loads it after the calling Lua task returns. Calls made directly from
`init.lua` wait in the startup queue until package installation is complete.

A trigger table loads the package the first time an event, command, or key is
used:

```lua
maki.pack.add({ "https://github.com/example/maki-goal" }, {
  load = {
    event = { "TurnStart" },
    cmd = { "/goal" },
    keys = { "<C-g>" },
  },
})
```

Event triggers accept the built-in turn, tool, and session events. Package
change events and plugin-defined events do not reach a dormant package. Maki
does not use Lua module misses as triggers because each package has its own
module root and permission owner.

In a mode that does not deliver agent events, an event-triggered package loads
at startup. Command-only and key-only packages stay dormant.

For full control, `load` can be a function. It receives the normalized
specification and installed path. Credentials in the source are redacted. The
function is then responsible for loading or configuring the package. See
`maki.pack.add` in the [Lua API](/docs/lua-api/) for the exact callback value.

## Inspect, update, and remove

Read current package records from `init.lua` or a package:

```lua
local all = maki.pack.get()
local goal = maki.pack.get({ "maki-goal" })[1]
```

Each record has `spec`, `path`, `rev`, and `active` fields. `path` is absent when
the recorded revision is not on disk. The original `data` value is available as
`record.spec.data`.

Use the command palette for normal maintenance:

```text
/packupdate                 review updates for all declared packages
/packupdate maki-goal       review one package
/packupdate! maki-goal      update without the review prompt
/packupdate ++offline       use Git refs that are already on disk
/packdel maki-goal          remove an inactive, undeclared package
/packdel! maki-goal         unload and remove an active, undeclared package
/packdel ++all              remove all inactive, undeclared packages
```

An update shows the old and new revisions and the available commit subjects
before it asks to apply. A failure before the change, or a declined update,
leaves the lockfile and active owner unchanged. If the new entrypoint fails,
the new revision stays recorded and the owner stays inactive. A dormant or
triggered package can be activated again; an eager package waits for the next
reload. Maki refuses to remove an active package without the force form. It
also stops deletion if owner cleanup fails.

If Maki cannot write the lockfile after a change, it restores the previous
approval state and package revision when possible. The old lockfile remains
authoritative.

Remove a package from `maki.pack.add` before you delete it. Maki rejects a
deletion while the package is still declared because the next reload would
install it again.

The Lua forms are available in `init.lua`:

```lua
maki.pack.update({ "maki-goal" }, { force = true })
maki.pack.del({ "maki-goal" }, { force = true })
```

For a batch, all `PackChangedPre` events fire before its first change. The
`PackChanged` events fire after successful changes are recorded. Event data has
`active`, `kind`, `spec`, and `path`.

## Install by hand

You can also clone a package into a Neovim-style package directory:

```text
<maki-data>/site/pack/<group>/start/<name>/
<maki-data>/site/pack/<group>/opt/<name>/
```

A `start` package loads at startup. An `opt` package waits for
`maki.packadd("name")`. A manual package gets the permissions that its local
`plugin.toml` requests because you placed the files yourself.

A package directory can contain sorted `plugin/*.lua` entry files and modules
at `lua/<module>.lua` or `lua/<module>/init.lua`.

Managed packages use immutable revision directories under
`site/pack/core/<name>/<commit>/`. Maki preserves relative symbolic links whose
targets stay inside the package. It skips absolute links and links that leave
the package. It does not fetch submodule working trees. Install a package by
hand if it needs an external link or a submodule.
